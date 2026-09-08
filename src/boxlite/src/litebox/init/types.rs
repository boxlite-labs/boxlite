//! Type definitions for initialization pipeline.

use crate::BoxID;
use crate::disk::Disk;
#[cfg(target_os = "linux")]
use crate::fs::BindMountHandle;
use crate::images::ContainerImageConfig;
use crate::litebox::config::BoxConfig;
use crate::litebox::ports::LivePublishedPorts;
use crate::portal::GuestSession;
use crate::portal::interfaces::ContainerRootfsInitConfig;
use crate::runtime::layout::BoxFilesystemLayout;
use crate::runtime::options::VolumeSpec;
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::vmm::{PreparedKernel, controller::VmmHandler};
use crate::volumes::{ContainerMount, GuestVolumeManager, VolumeShare, classify_volume_share};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

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
    /// Owner UID of host directory (for auto-idmap in guest).
    pub owner_uid: u32,
    /// Owner GID of host directory (for auto-idmap in guest).
    pub owner_gid: u32,
    /// For a single-file mount, the file's name (staged and bind-mounted on its
    /// own); `None` for a whole-directory mount.
    pub subpath: Option<String>,
}

pub fn resolve_user_volumes(volumes: &[VolumeSpec]) -> BoxliteResult<Vec<ResolvedVolume>> {
    let mut resolved = Vec::with_capacity(volumes.len());

    for (i, vol) in volumes.iter().enumerate() {
        // A reference should already be a directory by now: the local runtime
        // resolves it against its store at create (`resolve_managed_volumes`,
        // in `create_inner`), and a REST runtime never sends one this far.
        // Reaching here means a spec skipped that step, so say that rather
        // than let it fall through to the host-path checks below and surface
        // as "host path does not exist: my-data", which points the caller at
        // their own filesystem.
        if let Some(reference) = &vol.managed_volume {
            return Err(BoxliteError::Internal(format!(
                "managed volume {reference:?} reached boot unresolved; it should have been \
                 resolved to a directory when the box was created"
            )));
        }

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

        // A directory is shared as-is; a single file keeps its own path here and
        // is staged into a dedicated share dir later (see vmm_spawn), so virtio-fs
        // never exposes the file's host siblings.
        let (source_path, subpath) = match classify_volume_share(&resolved_path) {
            Some(VolumeShare::Dir(dir)) => (dir, None),
            Some(VolumeShare::File(name)) => (resolved_path.clone(), Some(name)),
            None => {
                return Err(BoxliteError::Config(format!(
                    "Volume host path is not a file or directory: {}",
                    vol.host_path
                )));
            }
        };

        let tag = format!("uservol{}", i);

        // Owner comes from the mount target itself (file or dir) for guest idmap.
        let (owner_uid, owner_gid) = {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&resolved_path).map_err(|e| {
                BoxliteError::Config(format!(
                    "Failed to stat volume path '{}': {}",
                    resolved_path.display(),
                    e
                ))
            })?;
            (meta.uid(), meta.gid())
        };

        tracing::debug!(
            tag = %tag,
            host_path = %source_path.display(),
            subpath = ?subpath,
            guest_path = %vol.guest_path,
            read_only = vol.read_only,
            owner_uid,
            owner_gid,
            "Resolved user volume"
        );

        resolved.push(ResolvedVolume {
            tag,
            host_path: source_path,
            guest_path: vol.guest_path.clone(),
            read_only: vol.read_only,
            owner_uid,
            owner_gid,
            subpath,
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

/// Boot assets prepared by the init pipeline for `VmmSpawnTask`.
#[derive(Clone, Debug, Default)]
pub struct PreparedBootAssets {
    pub kernel: Option<PreparedKernel>,
}

/// RAII guard for cleanup on initialization failure.
///
/// On drop (when armed):
///   1. stops the VM handler if started,
///   2. preserves on-disk diagnostic files (intentional — line 201 comment),
///   3. marks the box as `Failed` with `error_reason` so the record survives
///      for retry/inspection (canonical pattern: Daytona ERROR, Kata startVM
///      defer, containerd status.ExitCode, Docker SetError+CheckpointTo),
///   4. increments the failure counter.
///
/// The caller is expected to call `set_last_error()` before the error
/// propagates so Drop can record what went wrong.
pub struct CleanupGuard {
    runtime: SharedRuntimeImpl,
    box_id: BoxID,
    layout: Option<BoxFilesystemLayout>,
    handler: Option<Box<dyn VmmHandler>>,
    armed: bool,
    /// Captured cause for the eventual `Failed` state. Populated by the init
    /// pipeline caller via `set_last_error()` before the error propagates.
    /// `None` if Drop fires without an explicit cause — falls back to a
    /// generic placeholder in that case.
    last_error: Option<String>,
}

impl CleanupGuard {
    pub fn new(runtime: SharedRuntimeImpl, box_id: BoxID) -> Self {
        Self {
            runtime,
            box_id,
            layout: None,
            handler: None,
            armed: true,
            last_error: None,
        }
    }

    /// Capture the error that caused init to fail.
    ///
    /// Call this immediately before propagating the error out of the init
    /// pipeline. Stores `err.to_string()` so we don't need `Clone` on
    /// `BoxliteError`.
    pub fn set_last_error(&mut self, err: &BoxliteError) {
        self.last_error = Some(err.to_string());
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

    /// Get the PID of the VM subprocess, if a handler is registered.
    pub fn handler_pid(&self) -> Option<u32> {
        self.handler.as_ref().map(|h| h.pid())
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

        let reason = self
            .last_error
            .as_deref()
            .unwrap_or("box initialization failed (no cause captured)");

        tracing::warn!(box_id = %self.box_id, reason = %reason, "Box initialization failed, cleaning up");

        // Stop handler if started
        if let Some(ref mut handler) = self.handler
            && let Err(e) = handler.stop()
        {
            tracing::warn!("Failed to stop handler during cleanup: {}", e);
        }

        // DON'T cleanup filesystem - preserve diagnostic files for debugging
        if let Some(ref layout) = self.layout {
            tracing::error!(
                "Box failed. Diagnostic files preserved at:\n  {}\n\nTo destroy: issue DESTROY_SANDBOX or `boxlite rm {}`",
                layout.root().display(),
                self.box_id
            );
        }

        // Preserve the box record in the DB with status=Failed + error_reason.
        // Canonical pattern across Daytona / Kata / containerd / Docker:
        //   "persistent records survive init failure; only ephemeral runtime
        //    artifacts are torn down. Deletion is user-initiated."
        // Replaces the previous unconditional remove_box() which silently
        // orphaned on-disk state and lost the user's sandbox.
        match self.runtime.box_manager.update_box(&self.box_id) {
            Ok(mut state) => {
                state.mark_failed(reason);
                if let Err(e) = self.runtime.box_manager.save_box(&self.box_id, &state) {
                    tracing::warn!(
                        box_id = %self.box_id,
                        "Failed to persist Failed state during cleanup: {}", e
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    box_id = %self.box_id,
                    "Could not load state to mark Failed (record may have been deleted concurrently): {}", e
                );
            }
        }

        // Increment failure counter (existing Prometheus metric).
        self.runtime
            .runtime_metrics
            .boxes_failed
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Initialization pipeline context.
///
/// Contains all inputs and outputs for pipeline tasks.
/// Tasks read from config/runtime and write to Option fields.
pub struct InitPipelineContext {
    pub config: BoxConfig,
    pub runtime: SharedRuntimeImpl,
    pub guard: CleanupGuard,
    pub reuse_rootfs: bool,
    /// Skip waiting for guest ready signal (for reattach to running box).
    pub skip_guest_wait: bool,

    pub layout: Option<BoxFilesystemLayout>,
    pub boot_assets: Option<PreparedBootAssets>,
    pub container_image_config: Option<ContainerImageConfig>,
    pub container_disk: Option<Disk>,
    pub guest_disk: Option<Disk>,
    pub volume_mgr: Option<GuestVolumeManager>,
    pub rootfs_init: Option<ContainerRootfsInitConfig>,
    pub container_mounts: Option<Vec<ContainerMount>>,
    pub guest_session: Option<GuestSession>,
    /// The box's one network backend (set by vmm_spawn on first start/restart, or
    /// by vmm_attach on reattach; moved into LiveState for runtime control).
    pub network_backend: Option<Box<dyn crate::net::NetworkBackend>>,
    /// Concrete bindings confirmed by the port-publication stage. This remains
    /// `None` when a running legacy shim cannot expose its listener state or
    /// when best-effort reattachment reconciliation fails.
    pub published_ports: Option<LivePublishedPorts>,
    /// MITM CA cert PEM (set by vmm_spawn, read by guest_init for Container.Init gRPC).
    pub ca_cert_pem: Option<String>,

    #[cfg(target_os = "linux")]
    pub bind_mount: Option<BindMountHandle>,
}

impl InitPipelineContext {
    pub fn new(
        config: BoxConfig,
        runtime: SharedRuntimeImpl,
        reuse_rootfs: bool,
        skip_guest_wait: bool,
    ) -> Self {
        let guard = CleanupGuard::new(runtime.clone(), config.id.clone());
        Self {
            config,
            runtime,
            guard,
            reuse_rootfs,
            skip_guest_wait,
            layout: None,
            boot_assets: None,
            container_image_config: None,
            container_disk: None,
            guest_disk: None,
            volume_mgr: None,
            rootfs_init: None,
            container_mounts: None,
            guest_session: None,
            network_backend: None,
            published_ports: None,
            ca_cert_pem: None,
            #[cfg(target_os = "linux")]
            bind_mount: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::options::VolumeSpec;

    #[test]
    fn resolve_volume_gets_owner_uid() {
        let tmp = tempfile::tempdir().unwrap();
        let volumes = vec![VolumeSpec {
            managed_volume: None,
            host_path: tmp.path().to_str().unwrap().to_string(),
            guest_path: "/data".to_string(),
            read_only: false,
        }];

        let resolved = resolve_user_volumes(&volumes).unwrap();
        assert_eq!(resolved.len(), 1);

        // owner_uid should be the current user's UID
        use std::os::unix::fs::MetadataExt;
        let expected_uid = std::fs::metadata(tmp.path()).unwrap().uid();
        let expected_gid = std::fs::metadata(tmp.path()).unwrap().gid();

        assert_eq!(resolved[0].owner_uid, expected_uid);
        assert_eq!(resolved[0].owner_gid, expected_gid);
        assert_eq!(resolved[0].tag, "uservol0");
        assert_eq!(resolved[0].subpath, None);
    }

    #[test]
    fn resolve_volume_nonexistent_path_errors() {
        let volumes = vec![VolumeSpec::bind_mount("/nonexistent/path/12345", "/data")];

        let result = resolve_user_volumes(&volumes);
        assert!(result.is_err());
    }

    /// Boot never resolves a reference — the runtime does that at create. An
    /// unresolved one arriving here is a bug in that path, and the error has
    /// to say so; without the guard it falls through to the host-path checks
    /// and reports "host path does not exist: my-data", sending the caller
    /// looking for a directory they never asked for.
    #[test]
    fn resolve_managed_volume_reports_an_unresolved_reference() {
        let volumes = vec![VolumeSpec::managed_volume("my-data", "/data")];

        let error =
            resolve_user_volumes(&volumes).expect_err("an unresolved reference cannot be mounted");

        assert!(
            matches!(error, BoxliteError::Internal(_)),
            "{error:?} should be Internal, not a config error about a path"
        );
        let message = error.to_string();
        assert!(message.contains("my-data"), "{message}");
        assert!(message.contains("unresolved"), "{message}");
        assert!(
            !message.contains("host path"),
            "the error must not blame a host path: {message}"
        );
    }

    #[test]
    fn resolve_single_file_volume_records_source_and_name() {
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("app.conf");
        std::fs::write(&file_path, "key=value\n").unwrap();

        let volumes = vec![VolumeSpec {
            managed_volume: None,
            host_path: file_path.to_str().unwrap().to_string(),
            guest_path: "/etc/app.conf".to_string(),
            read_only: true,
        }];

        let resolved = resolve_user_volumes(&volumes).unwrap();
        assert_eq!(resolved.len(), 1);
        // Keeps the file's own path (staged into a share dir later) plus its name.
        assert_eq!(resolved[0].host_path, file_path.canonicalize().unwrap());
        assert_eq!(resolved[0].subpath, Some("app.conf".to_string()));
        assert_eq!(resolved[0].guest_path, "/etc/app.conf");
    }

    /// Reverting Drop to call `remove_box` (the pre-fix behavior) flips this red:
    /// `update_box` would return `NotFound` because the row was deleted.
    #[test]
    fn cleanup_guard_drop_persists_failed_state_and_keeps_record() {
        use crate::litebox::config::{BoxConfig, ContainerRuntimeConfig};
        use crate::runtime::id::BoxID;
        use crate::runtime::options::{BoxOptions, BoxliteOptions, RootfsSpec};
        use crate::runtime::rt_impl::RuntimeImpl;
        use crate::runtime::types::{BoxState, BoxStatus, ContainerID};
        use crate::vmm::VmmKind;
        use boxlite_test_utils::home::PerTestBoxHome;
        use chrono::Utc;
        use std::path::PathBuf;

        let home = PerTestBoxHome::isolated_in("/tmp");
        let runtime = RuntimeImpl::new(BoxliteOptions {
            home_dir: home.path.clone(),
            image_registries: vec![],
        })
        .expect("create runtime");

        let box_id = BoxID::parse("01HJK4TNRPQSXYZ8WM6NCVT9CG1").unwrap();
        let config = BoxConfig {
            id: box_id.clone(),
            name: None,
            created_at: Utc::now(),
            container: ContainerRuntimeConfig {
                id: ContainerID::new(),
            },
            options: BoxOptions {
                rootfs: RootfsSpec::Image("test:latest".to_string()),
                ..Default::default()
            },
            engine_kind: VmmKind::Libkrun,
            box_home: PathBuf::from("/tmp/box"),
        };
        runtime
            .box_manager
            .add_box(&config, &BoxState::new())
            .expect("seed Configured box");

        // Capture the Display string from production's BoxliteError so the
        // assertion below is on data routed through production code, not on
        // a literal the test body invented.
        let err =
            BoxliteError::Engine("Box CL84LvGx7RBE failed to start: timeout after 30s".to_string());
        let err_display = err.to_string();

        {
            let mut guard = CleanupGuard::new(runtime.clone(), box_id.clone());
            guard.set_last_error(&err);
            // Drop fires here: armed=true by default.
        }

        // Assertion 1: record was NOT deleted (the original bug).
        assert!(
            runtime.box_manager.has_box(&box_id).unwrap(),
            "CleanupGuard::drop must preserve the box record"
        );

        // Assertion 2: state is Failed (production transitioned it).
        let persisted = runtime.box_manager.update_box(&box_id).unwrap();
        assert_eq!(persisted.status, BoxStatus::Failed);

        // Assertion 3: error_reason carries the BoxliteError's Display string,
        // having round-tripped through set_last_error -> Drop -> mark_failed ->
        // save_box -> load_state.
        let reason = persisted
            .error_reason
            .as_deref()
            .expect("error_reason populated by Drop");
        assert!(
            reason.contains(&err_display),
            "error_reason should round-trip BoxliteError::Display; got {reason:?}"
        );
    }
}
