//! Boxlite runtime library.
//!
//! This crate provides the host-side API for managing Boxlite sandboxes.

use std::sync::OnceLock;
use tracing_subscriber::EnvFilter;

// Global guard for tracing-appender to keep the writer thread alive
static LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

mod controller;
pub mod fs;
pub mod management;
pub mod metrics;
pub mod net;
pub mod portal;
pub mod vmm;

mod images;
pub mod litebox;
mod rootfs;
pub mod runtime;
pub mod util;
mod volumes;

pub use litebox::LiteBox;
pub use runtime::BoxliteRuntime;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
pub use litebox::{
    BoxCommand, ExecResult, ExecStderr, ExecStdin, ExecStdout, Execution, ExecutionId,
};
pub use management::{BoxID, BoxInfo, BoxState};
pub use metrics::{BoxMetrics, RuntimeMetrics};
use runtime::layout::FilesystemLayout;
pub use runtime::options::{BoxOptions, BoxliteOptions, RootfsSpec};
pub use runtime::types::ContainerId;

/// Initialize tracing for Boxlite using the provided filesystem layout.
///
/// Logs are written to `<layout.home_dir()>/logs/boxlite.log` with daily rotation.
/// Uses the `RUST_LOG` environment variable for filtering (defaults to `info`).
/// Idempotent: subsequent calls return immediately once initialized.
pub fn init_logging_for(layout: &FilesystemLayout) -> BoxliteResult<()> {
    let logs_dir = layout.logs_dir();
    std::fs::create_dir_all(&logs_dir).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create logs directory {}: {}",
            logs_dir.display(),
            e
        ))
    })?;

    let _ = LOG_GUARD.get_or_init(|| {
        let file_appender = tracing_appender::rolling::daily(logs_dir, "boxlite.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        let env_filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new("info"))
            .unwrap_or_else(|_| EnvFilter::new("info"));

        // If global default subscriber is already set, this will return an error.
        // We ignore it to avoid interfering with host-configured tracing.
        util::register_to_tracing(non_blocking, env_filter);

        guard
    });

    Ok(())
}
