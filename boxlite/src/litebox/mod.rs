//! LiteBox - Individual box lifecycle management
//!
//! Provides lazy initialization and execution capabilities for isolated boxes.

mod box_impl;
pub(crate) mod config;
mod exec;
mod init;
mod manager;
mod state;

pub use exec::{BoxCommand, ExecResult, ExecStderr, ExecStdin, ExecStdout, Execution, ExecutionId};
pub(crate) use manager::BoxManager;
pub use state::{BoxState, BoxStatus};

pub(crate) use box_impl::SharedBoxImpl;
pub(crate) use init::BoxBuilder;

use crate::metrics::BoxMetrics;
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::{BoxID, BoxInfo};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
pub use config::BoxConfig;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::OnceCell;

/// LiteBox - Handle to a box.
///
/// Thin wrapper around BoxImpl with lazy initialization.
/// All operations delegate to the inner implementation after ensuring it's ready.
///
/// Following the same pattern as BoxliteRuntime wrapping RuntimeImpl.
pub struct LiteBox {
    /// Box ID for quick access without locking.
    id: BoxID,
    /// Box name for quick access without locking.
    name: Option<String>,
    /// Runtime reference for building.
    runtime: SharedRuntimeImpl,
    /// Lazily initialized box implementation.
    inner: OnceCell<SharedBoxImpl>,
    /// Whether shutdown has been requested.
    is_shutdown: AtomicBool,
}

impl LiteBox {
    /// Create a LiteBox from ID and runtime.
    ///
    /// Does NOT initialize VM immediately. Use operations that require the VM
    /// to trigger lazy initialization.
    pub(crate) fn new(runtime: SharedRuntimeImpl, id: BoxID, name: Option<String>) -> Self {
        Self {
            id,
            name,
            runtime,
            inner: OnceCell::new(),
            is_shutdown: AtomicBool::new(false),
        }
    }

    // ========================================================================
    // Accessors - all delegate to BoxImpl
    // ========================================================================

    pub fn id(&self) -> &BoxID {
        &self.id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub async fn info(&self) -> BoxliteResult<BoxInfo> {
        Ok(self.box_impl().await?.info())
    }

    // ========================================================================
    // Operations - all delegate to BoxImpl
    // ========================================================================

    pub async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution> {
        self.box_impl().await?.exec(command).await
    }

    pub async fn metrics(&self) -> BoxliteResult<BoxMetrics> {
        self.box_impl().await?.metrics()
    }

    pub async fn stop(&self) -> BoxliteResult<()> {
        self.is_shutdown.store(true, Ordering::SeqCst);

        if let Some(inner) = self.inner.get() {
            inner.stop().await
        } else {
            // Box was never started - just update database, don't initialize VM
            let mut state = self.runtime.box_manager.update_box(&self.id)?;
            state.set_status(BoxStatus::Stopped);
            state.set_pid(None);
            self.runtime.box_manager.save_box(&self.id, &state)?;
            Ok(())
        }
    }

    /// Get the inner BoxImpl, initializing it if necessary.
    async fn box_impl(&self) -> BoxliteResult<&SharedBoxImpl> {
        self.inner.get_or_try_init(|| self.init_box_impl()).await
    }

    /// Initialize and return BoxImpl.
    async fn init_box_impl(&self) -> BoxliteResult<SharedBoxImpl> {
        let (config, state) = self
            .runtime
            .box_manager
            .lookup_box(&self.id)?
            .ok_or_else(|| BoxliteError::NotFound(format!("box {} not found", self.id)))?;

        let inner = if state.status == BoxStatus::Running {
            // Reattach to running box
            let pid = state
                .pid
                .ok_or_else(|| BoxliteError::InvalidState("Running box has no PID".into()))?;
            box_impl::BoxImpl::reconnect(config, state, Arc::clone(&self.runtime), pid)?
        } else {
            // Build new box (Starting or Stopped)
            let builder = BoxBuilder::new(Arc::clone(&self.runtime), config, state)?;
            builder.build().await?
        };

        Ok(Arc::new(inner))
    }
}

// ============================================================================
// THREAD SAFETY ASSERTIONS
// ============================================================================

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<LiteBox>;
};
