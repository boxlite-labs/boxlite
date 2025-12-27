//! Box implementation - holds initialized box state and VM resources.

use crate::disk::Disk;
#[cfg(target_os = "linux")]
use crate::fs::BindMountHandle;
use crate::metrics::{BoxMetrics, BoxMetricsStorage};
use crate::portal::GuestSession;
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::runtime::types::BoxStatus;
use crate::vmm::controller::VmmHandler;
use crate::{BoxID, BoxInfo};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use parking_lot::RwLock;
use std::sync::atomic::Ordering;

use super::config::BoxConfig;
use super::exec::{BoxCommand, ExecStderr, ExecStdin, ExecStdout, Execution};
use super::state::BoxState;
use std::sync::Arc;

/// Shared reference to BoxImpl.
pub type SharedBoxImpl = Arc<BoxImpl>;

/// Box implementation - created lazily, holds all state after initialization.
pub(crate) struct BoxImpl {
    // Core identity
    pub(crate) config: BoxConfig,
    pub(crate) state: RwLock<BoxState>,
    pub(crate) runtime: SharedRuntimeImpl,

    // VM resources
    handler: std::sync::Mutex<Box<dyn VmmHandler>>,
    guest_session: GuestSession,
    metrics: BoxMetricsStorage,
    _container_rootfs_disk: Disk,
    #[allow(dead_code)]
    guest_rootfs_disk: Option<Disk>,
    container_id: String,
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    bind_mount: Option<BindMountHandle>,
}

impl BoxImpl {
    // ========================================================================
    // Accessors
    // ========================================================================

    pub fn id(&self) -> &BoxID {
        &self.config.id
    }

    pub fn name(&self) -> Option<&str> {
        self.config.name.as_deref()
    }

    pub fn config(&self) -> &BoxConfig {
        &self.config
    }

    pub fn status(&self) -> BoxStatus {
        self.state.read().status
    }

    pub fn auto_remove(&self) -> bool {
        self.config.options.auto_remove
    }

    pub fn info(&self) -> BoxInfo {
        let state = self.state.read();
        BoxInfo::new(&self.config, &state)
    }

    // ========================================================================
    // State management
    // ========================================================================

    pub fn save(&self) -> BoxliteResult<()> {
        let state = self.state.read();
        self.runtime.box_manager.save_box(&self.config.id, &state)?;
        Ok(())
    }

    pub fn set_status(&self, status: BoxStatus) {
        self.state.write().set_status(status);
    }

    pub fn set_pid(&self, pid: Option<u32>) {
        self.state.write().set_pid(pid);
    }

    // ========================================================================
    // Operations
    // ========================================================================

    pub async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution> {
        use boxlite_shared::constants::executor as executor_const;

        let command = if command
            .env
            .as_ref()
            .map(|env| env.iter().any(|(k, _)| k == executor_const::ENV_VAR))
            .unwrap_or(false)
        {
            command
        } else {
            command.env(
                executor_const::ENV_VAR,
                format!("{}={}", executor_const::CONTAINER_KEY, self.container_id),
            )
        };

        let mut exec_interface = self.guest_session.execution().await?;
        let result = exec_interface.exec(command).await;

        // Instrument metrics
        self.metrics.increment_commands_executed();
        self.runtime
            .runtime_metrics
            .total_commands
            .fetch_add(1, Ordering::Relaxed);

        if result.is_err() {
            self.metrics.increment_exec_errors();
            self.runtime
                .runtime_metrics
                .total_exec_errors
                .fetch_add(1, Ordering::Relaxed);
        }

        let components = result?;
        Ok(Execution::new(
            components.execution_id,
            exec_interface,
            components.result_rx,
            Some(ExecStdin::new(components.stdin_tx)),
            Some(ExecStdout::new(components.stdout_rx)),
            Some(ExecStderr::new(components.stderr_rx)),
        ))
    }

    pub fn metrics(&self) -> BoxliteResult<BoxMetrics> {
        let handler = self
            .handler
            .lock()
            .map_err(|e| BoxliteError::Internal(format!("handler lock poisoned: {}", e)))?;
        let raw = handler.metrics()?;

        Ok(BoxMetrics::from_storage(
            &self.metrics,
            raw.cpu_percent,
            raw.memory_bytes,
            None,
            None,
            None,
            None,
        ))
    }

    pub async fn stop(&self) -> BoxliteResult<()> {
        // Gracefully shut down guest
        if let Ok(mut guest) = self.guest_session.guest().await {
            let _ = guest.shutdown().await;
        }

        // Stop handler
        if let Ok(mut handler) = self.handler.lock() {
            handler.stop()?;
        }

        self.set_status(BoxStatus::Stopped);
        self.set_pid(None);
        self.save()?;

        tracing::info!("Stopped box {}", self.id());

        if self.auto_remove() {
            self.runtime.remove_box(self.id(), false)?;
        }

        Ok(())
    }

    // ========================================================================
    // Construction (called by BoxBuilder)
    // ========================================================================

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: BoxConfig,
        state: BoxState,
        runtime: SharedRuntimeImpl,
        handler: Box<dyn VmmHandler>,
        guest_session: GuestSession,
        metrics: BoxMetricsStorage,
        container_rootfs_disk: Disk,
        guest_rootfs_disk: Option<Disk>,
        container_id: String,
        #[cfg(target_os = "linux")] bind_mount: Option<BindMountHandle>,
    ) -> Self {
        Self {
            config,
            state: RwLock::new(state),
            runtime,
            handler: std::sync::Mutex::new(handler),
            guest_session,
            metrics,
            _container_rootfs_disk: container_rootfs_disk,
            guest_rootfs_disk,
            container_id,
            #[cfg(target_os = "linux")]
            bind_mount,
        }
    }

    /// Reconnect to an already-running box.
    pub(crate) fn reconnect(
        config: BoxConfig,
        state: BoxState,
        runtime: SharedRuntimeImpl,
        pid: u32,
    ) -> BoxliteResult<Self> {
        use crate::disk::DiskFormat;
        use crate::vmm::controller::ShimHandler;

        let container_id = state
            .container_id
            .clone()
            .ok_or_else(|| BoxliteError::InvalidState("Running box has no container_id".into()))?;

        let handler = ShimHandler::from_pid(pid, config.id.clone());
        let guest_session = GuestSession::new(config.transport.clone());
        let disk = Disk::new(config.box_home.join("root.qcow2"), DiskFormat::Qcow2, true);

        Ok(Self {
            config,
            state: RwLock::new(state),
            runtime,
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
