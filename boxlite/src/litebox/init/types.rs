//! Type definitions for initialization pipeline.

use crate::BoxID;
use crate::disk::Disk;
#[cfg(target_os = "linux")]
use crate::fs::BindMountHandle;
use crate::images::ContainerConfig;
use crate::litebox::BoxStatus;
use crate::litebox::config::BoxConfig;
use crate::portal::GuestSession;
use crate::portal::interfaces::ContainerRootfsInitConfig;
use crate::runtime::RuntimeInner;
use crate::runtime::guest_rootfs::GuestRootfs;
use crate::runtime::layout::BoxFilesystemLayout;
use crate::runtime::options::{BoxOptions, VolumeSpec};
use crate::runtime::types::{BoxState, ContainerId};
use crate::vmm::controller::VmmHandler;
use crate::volumes::{ContainerMount, GuestVolumeManager};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::OnceCell;

/// Switch between merged and overlayfs rootfs strategies.
/// - true: overlayfs (allows COW writes, keeps layers separate)
/// - false: merged rootfs (all layers merged on host)
pub const USE_OVERLAYFS: bool = true;

/// Switch to disk-based rootfs strategy.
/// - true: create ext4 disk from layers, use qcow2 COW overlay per box
/// - false: use virtiofs + overlayfs (default)
///
/// Disk-based rootfs is faster to start but requires more disk space.
/// When enabled, USE_OVERLAYFS is ignored.
pub const USE_DISK_ROOTFS: bool = true;

/// User-specified volume with resolved paths and generated tag.
#[derive(Debug, Clone)]
pub struct ResolvedVolume {
    pub tag: String,
    pub host_path: PathBuf,
    pub guest_path: String,
    pub read_only: bool,
}

pub fn resolve_user_volumes(volumes: &[VolumeSpec]) -> BoxliteResult<Vec<ResolvedVolume>> {
    let mut resolved = Vec::with_capacity(volumes.len());

    for (i, vol) in volumes.iter().enumerate() {
        let host_path = PathBuf::from(&vol.host_path);

        if !host_path.exists() {
            return Err(BoxliteError::Config(format!(
                "Volume host path does not exist: {}",
                vol.host_path
            )));
        }

        let resolved_path = host_path.canonicalize().map_err(|e| {
            BoxliteError::Config(format!(
                "Failed to resolve volume path '{}': {}",
                vol.host_path, e
            ))
        })?;

        if !resolved_path.is_dir() {
            return Err(BoxliteError::Config(format!(
                "Volume host path is not a directory: {}",
                vol.host_path
            )));
        }

        let tag = format!("uservol{}", i);

        tracing::debug!(
            tag = %tag,
            host_path = %resolved_path.display(),
            guest_path = %vol.guest_path,
            read_only = vol.read_only,
            "Resolved user volume"
        );

        resolved.push(ResolvedVolume {
            tag,
            host_path: resolved_path,
            guest_path: vol.guest_path.clone(),
            read_only: vol.read_only,
        });
    }

    Ok(resolved)
}

/// Result of rootfs preparation - either merged, separate layers, or disk image.
#[derive(Debug)]
pub enum ContainerRootfsPrepResult {
    /// Single merged directory (all layers merged on host)
    #[allow(dead_code)]
    Merged(PathBuf),
    /// Layers for guest-side overlayfs
    #[allow(dead_code)] // Overlayfs mode currently disabled (USE_DISK_ROOTFS=true)
    Layers {
        /// Parent directory containing all extracted layers (mount as single virtiofs share)
        layers_dir: PathBuf,
        /// Subdirectory names for each layer (e.g., "sha256-xxxx")
        layer_names: Vec<String>,
    },
    /// Disk image containing the complete rootfs
    /// The disk is attached as a block device and mounted directly
    DiskImage {
        /// Path to the base ext4 disk image (cached, shared across boxes)
        base_disk_path: PathBuf,
        /// Size of the disk in bytes (for creating COW overlay)
        disk_size: u64,
    },
}

/// RAII guard for cleanup on initialization failure.
///
/// Automatically cleans up resources and increments failure counter
/// if dropped without being disarmed.
pub struct CleanupGuard {
    runtime: RuntimeInner,
    box_id: BoxID,
    layout: Option<BoxFilesystemLayout>,
    handler: Option<Box<dyn VmmHandler>>,
    armed: bool,
}

impl CleanupGuard {
    pub fn new(runtime: RuntimeInner, box_id: BoxID) -> Self {
        Self {
            runtime,
            box_id,
            layout: None,
            handler: None,
            armed: true,
        }
    }

    /// Register layout for cleanup on failure.
    pub fn set_layout(&mut self, layout: BoxFilesystemLayout) {
        self.layout = Some(layout);
    }

    /// Register handler for cleanup on failure.
    pub fn set_handler(&mut self, handler: Box<dyn VmmHandler>) {
        self.handler = Some(handler);
    }

    /// Take ownership of handler (for success path).
    pub fn take_handler(&mut self) -> Option<Box<dyn VmmHandler>> {
        self.handler.take()
    }

    /// Disarm the guard (call on success).
    ///
    /// After disarming, Drop will not perform cleanup.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        tracing::warn!("Box initialization failed, cleaning up");

        // Stop handler if started
        if let Some(ref mut handler) = self.handler
            && let Err(e) = handler.stop()
        {
            tracing::warn!("Failed to stop handler during cleanup: {}", e);
        }

        // Cleanup filesystem
        if let Some(ref layout) = self.layout
            && let Err(e) = layout.cleanup()
        {
            tracing::warn!("Failed to cleanup box directory: {}", e);
        }

        // Remove from BoxManager (which handles DB delete via database-first pattern)
        // First mark as crashed so remove() doesn't fail the active check
        let _ = self.runtime.box_manager.mark_crashed(&self.box_id);
        if let Err(e) = self.runtime.box_manager.remove(&self.box_id) {
            tracing::warn!("Failed to remove box from manager during cleanup: {}", e);
        }

        // Increment failure counter
        self.runtime
            .runtime_metrics
            .boxes_failed
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Shared initialization pipeline context.
///
/// Stores shared inputs, outputs, and timing across all tasks.
pub struct InitPipelineContext {
    pub config: BoxConfig,
    pub state: BoxState,
    pub home_dir: PathBuf,
    pub runtime: RuntimeInner,
    pub guest_rootfs_cell: Arc<OnceCell<GuestRootfs>>,
    pub container_id: ContainerId,
    pub guard: CleanupGuard,
    pub fs_output: Option<FilesystemOutput>,
    pub rootfs_output: Option<ContainerRootfsOutput>,
    pub guest_rootfs_output: Option<GuestRootfsOutput>,
    pub config_output: Option<ConfigOutput>,
    pub guest_session: Option<GuestSession>,
    pub guest_output: Option<GuestOutput>,
    /// Container config extracted from image (set by VmmSpawnTask for GuestInitTask)
    pub container_config: Option<ContainerConfig>,
}

impl InitPipelineContext {
    pub fn new(
        config: BoxConfig,
        state: BoxState,
        home_dir: PathBuf,
        runtime: RuntimeInner,
        guest_rootfs_cell: Arc<OnceCell<GuestRootfs>>,
        container_id: ContainerId,
    ) -> Self {
        let guard = CleanupGuard::new(runtime.clone(), config.id.clone());
        Self {
            config,
            state,
            home_dir,
            runtime,
            guest_rootfs_cell,
            container_id,
            guard,
            fs_output: None,
            rootfs_output: None,
            guest_rootfs_output: None,
            config_output: None,
            guest_session: None,
            guest_output: None,
            container_config: None,
        }
    }

    pub fn should_reuse_rootfs(&self) -> bool {
        self.state.status == BoxStatus::Stopped
    }
}

// ============================================================================
// STAGE INPUT/OUTPUT TYPES
// ============================================================================

/// Input for filesystem stage.
pub struct FilesystemInput<'a> {
    pub box_id: &'a BoxID,
    pub runtime: &'a RuntimeInner,
    pub isolate_mounts: bool,
}

/// Output from filesystem stage.
pub struct FilesystemOutput {
    pub layout: BoxFilesystemLayout,
    /// Bind mount handle for mounts/ → shared/ binding (when isolate_mounts is enabled).
    /// Kept alive for the duration of box lifecycle; cleaned up on drop.
    #[cfg(target_os = "linux")]
    pub bind_mount: Option<BindMountHandle>,
}

/// Input for container rootfs stage.
pub struct ContainerRootfsInput<'a> {
    pub options: &'a BoxOptions,
    pub runtime: &'a RuntimeInner,
    /// Box filesystem layout (for disk paths)
    pub layout: &'a BoxFilesystemLayout,
    /// When true, reuse existing COW disk (for restart).
    pub reuse_rootfs: bool,
}

/// Output from container rootfs stage.
pub struct ContainerRootfsOutput {
    pub container_config: ContainerConfig,
    /// COW disk for container rootfs (created or reused on restart)
    pub disk: Disk,
}

/// Input for guest rootfs stage.
pub struct GuestRootfsInput<'a> {
    pub runtime: &'a RuntimeInner,
    pub guest_rootfs_cell: &'a Arc<OnceCell<GuestRootfs>>,
    /// Box filesystem layout (for disk paths)
    pub layout: &'a BoxFilesystemLayout,
    /// When true, reuse existing COW disk (for restart).
    pub reuse_rootfs: bool,
}

/// Output from guest rootfs stage.
pub struct GuestRootfsOutput {
    pub guest_rootfs: GuestRootfs,
    /// COW disk for guest rootfs (created or reused on restart)
    pub disk: Option<Disk>,
}

/// Output from config stage.
pub struct ConfigOutput {
    pub box_config: crate::vmm::InstanceSpec,
    /// Primary disk - in DiskImage mode, this is the rootfs disk (COW overlay of base ext4)
    pub disk: Disk,
    /// Init rootfs COW disk (protects shared base from writes)
    pub init_disk: Option<Disk>,
    /// Configured volume manager - guest_init calls build_guest_mounts()
    pub volume_mgr: GuestVolumeManager,
    /// Rootfs initialization config
    pub rootfs_init: ContainerRootfsInitConfig,
    /// Container bind mounts (user volumes)
    pub container_mounts: Vec<ContainerMount>,
}

/// Input for guest initialization stage.
pub struct GuestInput {
    pub guest_session: GuestSession,
    pub container_config: ContainerConfig,
    /// Container ID (generated by host).
    pub container_id: ContainerId,
    /// Configured volume manager - builds guest volumes
    pub volume_mgr: GuestVolumeManager,
    /// Rootfs initialization config
    pub rootfs_init: ContainerRootfsInitConfig,
    /// Container bind mounts (user volumes)
    pub container_mounts: Vec<ContainerMount>,
}

/// Output from guest initialization stage.
pub struct GuestOutput {
    pub container_id: String,
    pub guest_session: GuestSession,
}
