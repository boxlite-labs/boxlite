//! Universal Box runner binary for all engine types.
//!
//! This binary handles the actual Box execution in a subprocess and delegates
//! to the appropriate VMM based on the engine type argument.
//!
//! Engine implementations auto-register themselves via the inventory pattern,
//! so this runner doesn't need to know about specific engine types.

#[allow(unused_imports)]
use std::process;

use std::path::Path;

use boxlite::{
    runtime::constants,
    util,
    vmm::{self, InstanceSpec, VmmConfig, VmmKind},
};
use boxlite_shared::errors::BoxliteResult;
use clap::Parser;
#[allow(unused_imports)]
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

/// Universal Box runner binary - subprocess that executes isolated Boxes
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "BoxLite shim process - handles Box in isolated subprocess"
)]
struct ShimArgs {
    /// Engine type to use for Box execution
    ///
    /// Supported engines: libkrun, firecracker
    #[arg(long)]
    engine: VmmKind,

    /// Box configuration as JSON string
    ///
    /// This contains the full InstanceSpec including rootfs path, volumes,
    /// networking, guest entrypoint, and other runtime configuration.
    #[arg(long)]
    config: String,
}

/// Initialize tracing with file logging.
///
/// Logs are written to {home_dir}/logs/boxlite-shim.log with daily rotation.
/// Returns WorkerGuard that must be kept alive to maintain the background writer thread.
fn init_logging(home_dir: &Path) -> tracing_appender::non_blocking::WorkerGuard {
    let logs_dir = home_dir.join(constants::dirs::LOGS_DIR);

    // Create logs directory if it doesn't exist
    std::fs::create_dir_all(&logs_dir).expect("Failed to create logs directory");

    // Set up file appender with daily rotation
    let file_appender = tracing_appender::rolling::daily(logs_dir, "boxlite-shim.log");

    // Create non-blocking writer
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Set up env filter (defaults to "info" if RUST_LOG not set)
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    // Initialize subscriber with file output
    util::register_to_tracing(non_blocking, env_filter);

    guard
}

fn main() -> BoxliteResult<()> {
    // Parse command line arguments with clap
    // VmmKind parsed via FromStr trait automatically
    let args = ShimArgs::parse();

    // Parse InstanceSpec from JSON
    let config: InstanceSpec = serde_json::from_str(&args.config).map_err(|e| {
        boxlite_shared::errors::BoxliteError::Engine(format!("Failed to parse config JSON: {}", e))
    })?;

    // Initialize logging using home_dir from config
    // Keep guard alive until end of main to ensure logs are written
    let _log_guard = init_logging(&config.home_dir);

    tracing::info!(engine = ?args.engine, "Box runner starting");
    tracing::debug!(
        shares = ?config.fs_shares.shares(),
        "Filesystem shares configured"
    );
    tracing::debug!(
        entrypoint = ?config.guest_entrypoint.executable,
        "Guest entrypoint configured"
    );

    // Start parent process monitor thread
    // This ensures the runner exits if the parent process dies
    start_parent_monitor();

    // Initialize engine options with defaults
    let options = VmmConfig::default();

    // Create engine using inventory pattern (no match statement needed!)
    // Engines auto-register themselves at compile time
    let mut engine = vmm::create_engine(args.engine, options)?;

    tracing::info!("Engine created, creating Box instance");

    // Create Box instance with the provided configuration
    let instance = match engine.create(config) {
        Ok(instance) => instance,
        Err(e) => {
            tracing::error!("Failed to create Box instance: {}", e);
            return Err(e);
        }
    };

    tracing::info!("Box instance created, handing over process control to Box");

    // Hand over process control to Box instance
    // This may never return (process takeover)
    match instance.enter() {
        Ok(()) => {
            tracing::info!("Box execution completed successfully");
            Ok(())
        }
        Err(e) => {
            tracing::error!("Box execution failed: {}", e);
            Err(e)
        }
    }
}

/// Monitor parent process and exit if it dies.
/// This prevents orphaned Box processes when the parent (Python) crashes or is killed.
///
/// On Linux: Uses prctl(PR_SET_PDEATHSIG, SIGTERM) - kernel sends signal when parent dies
/// On macOS: Uses polling with getppid() - less elegant but reliable
#[cfg(target_os = "linux")]
fn start_parent_monitor() {
    // On Linux, use prctl to request SIGTERM when parent dies
    // This is the most elegant solution - kernel handles it automatically
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
            tracing::info!("Failed to set parent death signal");
        }
    }
}

#[cfg(target_os = "macos")]
fn start_parent_monitor() {
    // macOS doesn't have PR_SET_PDEATHSIG, so we poll
    use std::thread;
    use std::time::Duration;

    let parent_pid = unsafe { libc::getppid() };

    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(1));

            // Check if parent process still exists
            // getppid() returns 1 (init/launchd) if parent died
            let current_parent = unsafe { libc::getppid() };
            if current_parent != parent_pid {
                tracing::info!(parent_pid, "Parent process died, exiting");
                process::exit(1);
            }
        }
    });
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn start_parent_monitor() {
    // No-op on other systems
}
