//! LiteBox - Individual box lifecycle management
//!
//! Provides lazy initialization and execution capabilities for isolated boxes.
//!
//! ## Architecture
//!
//! This module is organized into focused submodules:
//! - `status`: Box lifecycle status and state machine
//! - `init`: Initialization orchestration (image pulling, rootfs prep, Box startup)
//! - `lifecycle`: State management (start/stop/destroy/cleanup)
//! - `exec`: Command execution
//! - `metrics`: Metrics collection and aggregation

pub(crate) mod config;
mod exec;
mod init;
mod inner;
mod lifecycle;
mod manager;
mod metrics;
mod state;

pub use exec::{BoxCommand, ExecResult, ExecStderr, ExecStdin, ExecStdout, Execution, ExecutionId};
pub(crate) use manager::BoxManager;
pub use state::{BoxState, BoxStatus};

pub(crate) use init::BoxBuilder;

use crate::metrics::BoxMetrics;
use crate::runtime::rt_impl::RuntimeInner;
use crate::{BoxID, BoxInfo};
use boxlite_shared::errors::BoxliteResult;
use config::BoxConfig;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// BoxHandle represents a running Box.
///
/// This handle provides access to the Box's execution capabilities
/// and lifecycle management through the universal subprocess architecture.
///
/// Conceptually, it plays the same role that `std::process::Child` does for
/// `std::process::Command`, giving callers control over the spawned execution.
///
/// **Design**: Holds `RuntimeInner` (Arc) for direct access to runtime state.
/// All state operations acquire the lock directly and call manager methods.
///
/// **Lazy Initialization**: Heavy initialization (images pulling, Box startup) is deferred
/// until the first API call that requires the box to be running.
///
/// **Restart Support**: Uses tokio::sync::RwLock<Option<Arc<BoxInner>>> instead of OnceCell
/// to allow replacing the inner state during restart operations.
pub struct LiteBox {
    id: BoxID,
    name: Option<String>,
    runtime: RuntimeInner,
    inner: tokio::sync::RwLock<Option<Arc<init::BoxInner>>>,
    builder: tokio::sync::Mutex<Option<BoxBuilder>>,
    is_shutdown: AtomicBool,
    /// Automatically remove box when stopped
    auto_remove: bool,
}

impl LiteBox {
    /// Create a LiteBox from config and state.
    ///
    /// Works for both new boxes (from create()) and recovered boxes (from get()).
    /// The state determines initialization mode:
    /// - `Starting`: new box, normal init
    /// - `Stopped`: restart, reuse existing rootfs
    /// - `Running`: reattach (handled in ensure_ready)
    ///
    /// **Internal Use**: Called by BoxliteRuntime::create() and BoxliteRuntime::get().
    pub(crate) fn new(
        runtime: RuntimeInner,
        config: BoxConfig,
        state: &BoxState,
    ) -> BoxliteResult<Self> {
        tracing::trace!(
            box_id = %config.id,
            status = ?state.status,
            "LiteBox::new called"
        );

        // Create builder for states that can use it
        let builder = if state.status.can_exec() {
            tracing::trace!(
                box_id = %config.id,
                status = ?state.status,
                "Status can_exec, creating BoxBuilder"
            );
            Some(BoxBuilder::new(
                Arc::clone(&runtime),
                config.clone(),
                state.clone(),
            )?)
        } else {
            tracing::trace!(
                box_id = %config.id,
                status = ?state.status,
                "Status cannot exec, no builder created"
            );
            None
        };

        tracing::trace!(box_id = %config.id, has_builder = builder.is_some(), "LiteBox created");

        let auto_remove = config.options.auto_remove;
        let name = config.name.clone();

        Ok(Self {
            id: config.id.clone(),
            name,
            runtime,
            inner: tokio::sync::RwLock::new(None),
            builder: tokio::sync::Mutex::new(builder),
            is_shutdown: AtomicBool::new(false),
            auto_remove,
        })
    }

    /// Get the unique identifier for this box.
    pub fn id(&self) -> &BoxID {
        &self.id
    }

    /// Get the user-defined name for this box (if any).
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Get current information about this box.
    pub fn info(&self) -> BoxliteResult<BoxInfo> {
        lifecycle::info(self)
    }

    /// Execute a command and return an Execution handle (NEW API).
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # async fn example(litebox: &boxlite::LiteBox) -> Result<(), Box<dyn std::error::Error>> {
    /// use boxlite::BoxCommand;
    /// use futures::StreamExt;
    ///
    /// let mut execution = litebox.exec(BoxCommand::new("ls").arg("-la")).await?;
    ///
    /// // Read stdout
    /// let mut stdout = execution.stdout.take().unwrap();
    /// while let Some(line) = stdout.next().await {
    ///     println!("{}", line);
    /// }
    ///
    /// let status = execution.wait().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution> {
        exec::exec(self, command).await
    }

    /// Get unified metrics (operational + system + network).
    ///
    /// Returns a snapshot of:
    /// - Operational metrics: Commands executed, errors, bytes transferred (monotonic counters)
    /// - System metrics: CPU usage, memory usage (current values)
    /// - Network metrics: Bandwidth, TCP connections, errors (from network backend)
    /// - Timing metrics: Spawn and boot duration
    ///
    /// All operational counters never reset - delta calculation is caller's responsibility.
    /// System and network metrics are fetched fresh on every call.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use boxlite::LiteBox;
    /// # async fn example(litebox: &LiteBox) -> Result<(), Box<dyn std::error::Error>> {
    /// let metrics = litebox.metrics().await?;
    /// println!("Commands executed: {}", metrics.commands_executed_total());
    /// println!("CPU usage: {:?}%", metrics.cpu_percent());
    /// println!("Memory: {:?} bytes", metrics.memory_bytes());
    /// println!("Boot time: {}ms", metrics.guest_boot_duration_ms().unwrap_or(0));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn metrics(&self) -> BoxliteResult<BoxMetrics> {
        metrics::metrics(self).await
    }

    /// Stop the box gracefully.
    ///
    /// The VM is stopped but the box directory and state are preserved.
    /// You can restart the box later by calling `exec()` on a new handle
    /// obtained via `runtime.get()`.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Box is not initialized
    /// - Box is not in a stoppable state
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # async fn example(litebox: boxlite::LiteBox, runtime: &boxlite::BoxliteRuntime) -> Result<(), Box<dyn std::error::Error>> {
    /// let box_id = litebox.id().clone();
    ///
    /// // Stop the box (preserves state)
    /// litebox.stop().await?;
    ///
    /// // Can restart later via get() + exec()
    /// let litebox = runtime.get(&box_id)?.unwrap();
    /// litebox.exec(boxlite::BoxCommand::new("echo").arg("restarted")).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop(&self) -> BoxliteResult<()> {
        lifecycle::stop(self).await
    }
}

impl Drop for LiteBox {
    fn drop(&mut self) {
        lifecycle::drop_handler(self)
    }
}

// ============================================================================
// THREAD SAFETY ASSERTIONS
// ============================================================================

// Compile-time assertions to ensure LiteBox is Send + Sync
// This is critical for multithreaded usage (e.g., Python GIL release)
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<LiteBox>;
};
