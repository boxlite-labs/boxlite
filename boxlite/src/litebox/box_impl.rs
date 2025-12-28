//! Box implementation - holds config, state, and lazily-initialized VM resources.

// ============================================================================
// IMPORTS
// ============================================================================

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;
use tokio::sync::OnceCell;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use crate::disk::Disk;
#[cfg(target_os = "linux")]
use crate::fs::BindMountHandle;
use crate::metrics::{BoxMetrics, BoxMetricsStorage};
use crate::portal::GuestSession;
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::runtime::types::BoxStatus;
use crate::vmm::controller::VmmHandler;
use crate::{BoxID, BoxInfo};

use super::config::BoxConfig;
use super::exec::{BoxCommand, ExecStderr, ExecStdin, ExecStdout, Execution};
use super::state::BoxState;

// ============================================================================
// TYPE ALIASES
// ============================================================================

/// Shared reference to BoxImpl.
pub type SharedBoxImpl = Arc<BoxImpl>;

// ============================================================================
// LIVE STATE
// ============================================================================

/// Live state - lazily initialized when VM is started.
///
/// Contains all resources related to a running VM instance.
/// Separated from BoxImpl to allow operations like `info()` without initializing LiveState.
pub(crate) struct LiveState {
    // VM process control
    handler: std::sync::Mutex<Box<dyn VmmHandler>>,
    guest_session: GuestSession,

    // Metrics
    metrics: BoxMetricsStorage,

    // Disk resources (kept for lifecycle management)
    _container_rootfs_disk: Disk,
    #[allow(dead_code)]
    guest_rootfs_disk: Option<Disk>,

    // Platform-specific
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    bind_mount: Option<BindMountHandle>,
}

impl LiveState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        handler: Box<dyn VmmHandler>,
        guest_session: GuestSession,
        metrics: BoxMetricsStorage,
        container_rootfs_disk: Disk,
        guest_rootfs_disk: Option<Disk>,
        #[cfg(target_os = "linux")] bind_mount: Option<BindMountHandle>,
    ) -> Self {
        Self {
            handler: std::sync::Mutex::new(handler),
            guest_session,
            metrics,
            _container_rootfs_disk: container_rootfs_disk,
            guest_rootfs_disk,
            #[cfg(target_os = "linux")]
            bind_mount,
        }
    }
}

// ============================================================================
// BOX IMPL
// ============================================================================

/// Box implementation - created immediately, holds config and state.
///
/// VM resources are held in LiveState and lazily initialized on first use.
pub(crate) struct BoxImpl {
    // --- Always available ---
    pub(crate) config: BoxConfig,
    pub(crate) state: RwLock<BoxState>,
    pub(crate) runtime: SharedRuntimeImpl,
    is_shutdown: AtomicBool,

    // --- Lazily initialized ---
    live: OnceCell<LiveState>,
}

impl BoxImpl {
    // ========================================================================
    // CONSTRUCTION
    // ========================================================================

    /// Create BoxImpl with config and state (LiveState not initialized yet).
    ///
    /// LiveState will be lazily initialized when operations requiring it are called.
    pub(crate) fn new(config: BoxConfig, state: BoxState, runtime: SharedRuntimeImpl) -> Self {
        Self {
            config,
            state: RwLock::new(state),
            runtime,
            is_shutdown: AtomicBool::new(false),
            live: OnceCell::new(),
        }
    }

    // ========================================================================
    // ACCESSORS (no LiveState required)
    // ========================================================================

    pub(crate) fn id(&self) -> &BoxID {
        &self.config.id
    }

    pub(crate) fn container_id(&self) -> &str {
        self.config.container.id.as_str()
    }

    pub(crate) fn info(&self) -> BoxInfo {
        let state = self.state.read();
        BoxInfo::new(&self.config, &state)
    }

    // ========================================================================
    // STATE MANAGEMENT (no LiveState required)
    // ========================================================================

    /// Update state locally and sync to database.
    fn update_state<F>(&self, f: F) -> BoxliteResult<()>
    where
        F: FnOnce(&mut BoxState),
    {
        let mut state = self.state.write();
        f(&mut state);
        self.runtime.box_manager.save_box(&self.config.id, &state)?;
        Ok(())
    }

    // ========================================================================
    // OPERATIONS (require LiveState)
    // ========================================================================

    pub(crate) async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution> {
        use boxlite_shared::constants::executor as executor_const;

        // Check if box is stopped before proceeding
        if self.is_shutdown.load(Ordering::SeqCst) {
            return Err(BoxliteError::InvalidState("Box is stopped".into()));
        }

        let live = self.live_state().await?;

        // Inject container ID into environment if not already set
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
                format!("{}={}", executor_const::CONTAINER_KEY, self.container_id()),
            )
        };

        let mut exec_interface = live.guest_session.execution().await?;
        let result = exec_interface.exec(command).await;

        // Instrument metrics
        live.metrics.increment_commands_executed();
        self.runtime
            .runtime_metrics
            .total_commands
            .fetch_add(1, Ordering::Relaxed);

        if result.is_err() {
            live.metrics.increment_exec_errors();
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

    pub(crate) async fn metrics(&self) -> BoxliteResult<BoxMetrics> {
        // Check if box is stopped before proceeding
        if self.is_shutdown.load(Ordering::SeqCst) {
            return Err(BoxliteError::InvalidState("Box is stopped".into()));
        }

        let live = self.live_state().await?;
        let handler = live
            .handler
            .lock()
            .map_err(|e| BoxliteError::Internal(format!("handler lock poisoned: {}", e)))?;
        let raw = handler.metrics()?;

        Ok(BoxMetrics::from_storage(
            &live.metrics,
            raw.cpu_percent,
            raw.memory_bytes,
            None,
            None,
            None,
            None,
        ))
    }

    pub(crate) async fn stop(&self) -> BoxliteResult<()> {
        self.is_shutdown.store(true, Ordering::SeqCst);

        // Only try to stop VM if LiveState exists
        if let Some(live) = self.live.get() {
            // Gracefully shut down guest
            if let Ok(mut guest) = live.guest_session.guest().await {
                let _ = guest.shutdown().await;
            }

            // Stop handler
            if let Ok(mut handler) = live.handler.lock() {
                handler.stop()?;
            }
        }

        // Update state in database
        self.update_state(|state| {
            state.set_status(BoxStatus::Stopped);
            state.set_pid(None);
        })?;

        // Invalidate cache so new handles get fresh BoxImpl
        self.runtime.invalidate_box_impl(self.id());

        tracing::info!("Stopped box {}", self.id());

        if self.config.options.auto_remove {
            self.runtime.remove_box(self.id(), false)?;
        }

        Ok(())
    }

    // ========================================================================
    // LIVE STATE INITIALIZATION (internal)
    // ========================================================================

    /// Get LiveState, lazily initializing it if needed.
    async fn live_state(&self) -> BoxliteResult<&LiveState> {
        self.live.get_or_try_init(|| self.init_live_state()).await
    }

    /// Initialize LiveState via BoxBuilder.
    ///
    /// BoxBuilder handles all status types with different execution plans:
    /// - Starting: full pipeline (filesystem, rootfs, spawn, connect, init)
    /// - Stopped: restart pipeline (reuse rootfs, spawn, connect, init)
    /// - Running: attach pipeline (attach, connect)
    async fn init_live_state(&self) -> BoxliteResult<LiveState> {
        use super::BoxBuilder;

        let state = self.state.read().clone();
        let builder = BoxBuilder::new(Arc::clone(&self.runtime), self.config.clone(), state)?;
        builder.build().await
    }
}
