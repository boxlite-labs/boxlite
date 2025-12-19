//! Type definitions for initialization pipeline.

use crate::BoxID;
use crate::controller::ShimController;
use crate::disk::Disk;
#[cfg(target_os = "linux")]
use crate::fs::BindMountHandle;
use crate::images::{ContainerConfig, ImageObject};
use crate::metrics::BoxMetricsStorage;
use crate::net::NetworkBackend;
use crate::portal::GuestSession;
use crate::runtime::RuntimeInner;
use crate::runtime::guest_rootfs::GuestRootfs;
use crate::runtime::layout::BoxFilesystemLayout;
use crate::runtime::options::{BoxOptions, VolumeSpec};
use crate::runtime::types::ContainerId;
use crate::vmm::VmmController;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use boxlite_shared::layout::SharedGuestLayout;
use std::path::PathBuf;
use std::sync::Arc;
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

/// Final initialized box state.
pub(crate) struct BoxInner {
    pub(in crate::litebox) box_home: PathBuf,
    pub(in crate::litebox) controller: std::sync::Mutex<Box<dyn VmmController>>,
    pub(in crate::litebox) guest_session: GuestSession,
    pub(in crate::litebox) network_backend: Option<Box<dyn NetworkBackend>>,
    /// Per-box operational metrics (stored internally, like Tokio's TaskMetrics)
    pub(in crate::litebox) metrics: BoxMetricsStorage,
    /// RAII-managed rootfs disk (COW overlay of base ext4, auto-cleanup on drop)
    pub(in crate::litebox) _container_rootfs_disk: Disk,
    /// RAII-managed init rootfs disk (auto-cleanup on drop)
    /// Note: This field is not read directly, but kept for RAII disk cleanup.
    #[allow(dead_code)]
    pub(in crate::litebox) guest_rootfs_disk: Option<Disk>,
    /// Container ID for exec requests (used in BOXLITE_EXECUTOR env var)
    pub(in crate::litebox) container_id: String,
    /// RAII-managed bind mount for mounts/ → shared/ (Linux only, auto-cleanup on drop)
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    pub(in crate::litebox) bind_mount: Option<BindMountHandle>,
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

/// Input for rootfs stage.
/// Note: No layout dependency - runs in parallel with filesystem stage.
pub struct RootfsInput<'a> {
    pub options: &'a BoxOptions,
    pub runtime: &'a RuntimeInner,
}

/// Output from rootfs stage.
pub struct RootfsOutput {
    pub container_config: ContainerConfig,
    pub rootfs_result: ContainerRootfsPrepResult,
    pub image: ImageObject,
}

/// Input for guest rootfs stage.
pub struct GuestRootfsInput<'a> {
    pub runtime: &'a RuntimeInner,
    pub guest_rootfs_cell: &'a Arc<OnceCell<GuestRootfs>>,
}

/// Output from guest rootfs stage.
pub struct GuestRootfsOutput {
    pub guest_rootfs: GuestRootfs,
}

/// Input for config stage.
pub struct ConfigInput<'a> {
    pub options: &'a BoxOptions,
    pub layout: &'a BoxFilesystemLayout,
    pub rootfs: &'a RootfsOutput,
    pub guest_rootfs: &'a GuestRootfs,
    pub home_dir: &'a PathBuf,
    pub container_id: &'a ContainerId,
}

/// Output from config stage.
pub struct ConfigOutput {
    pub box_config: crate::vmm::InstanceSpec,
    pub network_backend: Option<Box<dyn NetworkBackend>>,
    /// Primary disk - in DiskImage mode, this is the rootfs disk (COW overlay of base ext4)
    pub disk: Disk,
    pub user_volumes: Vec<ResolvedVolume>,
    /// Init rootfs COW disk (protects shared base from writes)
    pub init_disk: Option<Disk>,
    /// Device path for rootfs disk in guest (e.g., "/dev/vda")
    /// Only set for DiskImage mode
    pub rootfs_device_path: Option<String>,
}

/// Input for spawn stage.
pub struct SpawnInput<'a> {
    pub box_id: &'a BoxID,
    pub config: &'a crate::vmm::InstanceSpec,
}

/// Output from spawn stage.
pub struct SpawnOutput {
    pub controller: ShimController,
    pub guest_session: GuestSession,
}

/// Input for guest initialization stage.
pub struct GuestInput {
    pub guest_session: GuestSession,
    pub container_rootfs_result: ContainerRootfsPrepResult,
    pub container_config: ContainerConfig,
    pub user_volumes: Vec<ResolvedVolume>,
    /// Guest shared layout for constructing guest-side paths.
    pub guest_shared_layout: SharedGuestLayout,
    /// Container ID (generated by host).
    pub container_id: ContainerId,
    /// Device path for rootfs disk in guest (e.g., "/dev/vda")
    /// Only set for DiskImage mode
    pub rootfs_device_path: Option<String>,
}

/// Output from guest initialization stage.
pub struct GuestOutput {
    pub container_id: String,
    pub guest_session: GuestSession,
}
