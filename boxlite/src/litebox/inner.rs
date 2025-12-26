use crate::disk::Disk;
#[cfg(target_os = "linux")]
use crate::fs::BindMountHandle;
use crate::metrics::BoxMetricsStorage;
use crate::portal::GuestSession;
use crate::vmm::controller::VmmHandler;
use boxlite_shared::BoxliteResult;
use std::path::PathBuf;

/// Final initialized box state.
pub(crate) struct BoxInner {
    #[allow(dead_code)]
    pub(in crate::litebox) box_home: PathBuf,
    pub(in crate::litebox) handler: std::sync::Mutex<Box<dyn VmmHandler>>,
    pub(in crate::litebox) guest_session: GuestSession,
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

impl BoxInner {
    /// Reconnect to an already-running box.
    ///
    /// Creates BoxInner by attaching to an existing process and reconnecting
    /// to the guest session. Used for reattach after detach.
    ///
    /// # Arguments
    /// * `config` - Box configuration
    /// * `state` - Box state (must contain container_id)
    /// * `pid` - Process ID of the running box
    pub(crate) async fn reconnect(
        config: &crate::litebox::config::BoxConfig,
        state: &crate::runtime::types::BoxState,
        pid: u32,
    ) -> BoxliteResult<Self> {
        use crate::disk::DiskFormat;
        use boxlite_shared::BoxliteError;

        // Get container_id from state (required for exec)
        let container_id = state.container_id.clone().ok_or_else(|| {
            BoxliteError::InvalidState("Running box has no container_id in state".into())
        })?;

        use crate::vmm::controller::ShimHandler;

        // Attach to existing process (no spawn, no log_handler for reconnect)
        let handler = ShimHandler::from_pid(pid, config.id.clone());

        // Reconnect to guest session (lazy connection)
        let guest_session = GuestSession::new(config.transport.clone());

        // Create persistent disk handle (won't be deleted on drop)
        // The disk already exists in box_home from the original init
        let disk_path = config.box_home.join("root.qcow2");
        let disk = Disk::new(disk_path, DiskFormat::Qcow2, true); // persistent=true

        Ok(Self {
            box_home: config.box_home.clone(),
            handler: std::sync::Mutex::new(Box::new(handler)),
            guest_session,
            metrics: BoxMetricsStorage::new(),
            _container_rootfs_disk: disk,
            guest_rootfs_disk: None,
            container_id: container_id.to_string(),
            #[cfg(target_os = "linux")]
            bind_mount: None,
        })
    }
}
