//! LiteBox - Individual box lifecycle management
//!
//! Provides lazy initialization and execution capabilities for isolated boxes.

pub(crate) mod box_impl;
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
use crate::{BoxID, BoxInfo};
use boxlite_shared::errors::BoxliteResult;
pub use config::BoxConfig;

/// LiteBox - Handle to a box.
///
/// Thin wrapper around BoxImpl. BoxImpl is created immediately with config,
/// but VM resources (LiveState) are lazily initialized on first use.
///
/// Following the same pattern as BoxliteRuntime wrapping RuntimeImpl.
pub struct LiteBox {
    /// Box ID for quick access without locking.
    id: BoxID,
    /// Box name for quick access without locking.
    name: Option<String>,
    /// Box implementation (created immediately, LiveState is lazy).
    inner: SharedBoxImpl,
}

impl LiteBox {
    /// Create a LiteBox from a shared BoxImpl.
    ///
    /// Used by RuntimeImpl to create handles that share the same BoxImpl.
    /// Multiple handles to the same box share the same LiveState.
    pub(crate) fn new(inner: SharedBoxImpl) -> Self {
        let id = inner.id().clone();
        let name = inner.config.name.clone();
        Self { id, name, inner }
    }

    pub fn id(&self) -> &BoxID {
        &self.id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Get box info without triggering VM initialization.
    pub fn info(&self) -> BoxInfo {
        self.inner.info()
    }

    pub async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution> {
        self.inner.exec(command).await
    }

    pub async fn metrics(&self) -> BoxliteResult<BoxMetrics> {
        self.inner.metrics().await
    }

    pub async fn stop(&self) -> BoxliteResult<()> {
        self.inner.stop().await
    }
}

// ============================================================================
// THREAD SAFETY ASSERTIONS
// ============================================================================

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<LiteBox>;
};
