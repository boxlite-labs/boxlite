//! Universal Box runner binary for all engine types.
//!
//! This binary handles the actual Box execution in a subprocess and delegates
//! to the appropriate VMM based on the engine type argument.
//!
//! Engine implementations auto-register themselves via the inventory pattern,
//! so this runner doesn't need to know about specific engine types.
//!
//! ## Network Backend
//!
//! The shim creates the network backend (gvproxy) from network_config if present.
//! This ensures networking survives detach operations - the gvproxy lives in the
//! shim subprocess, not the main boxlite process.

use std::path::Path;

use boxlite::{
    runtime::{constants, layout},
    util,
    vmm::{self, InstanceSpec, VmmConfig, VmmKind},
};
use boxlite_shared::errors::BoxliteResult;
use clap::Parser;
#[allow(unused_imports)]
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

#[cfg(feature = "gvproxy-backend")]
use boxlite::net::{ConnectionType, NetworkBackendEndpoint, gvproxy::GvproxyInstance};

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
    let logs_dir = home_dir.join(layout::dirs::LOGS_DIR);

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
    let mut config: InstanceSpec = serde_json::from_str(&args.config).map_err(|e| {
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

    // Create network backend (gvproxy) from network_config if present.
    // gvproxy provides virtio-net (eth0) to the guest - required even without port mappings.
    // The gvproxy instance is leaked intentionally - it must live for the entire
    // duration of the VM. When the shim process exits, OS cleans up all resources.
    #[cfg(feature = "gvproxy-backend")]
    if let Some(ref net_config) = config.network_config {
        tracing::info!(
            port_mappings = ?net_config.port_mappings,
            "Creating network backend (gvproxy) from config"
        );

        // Create gvproxy instance
        let gvproxy = GvproxyInstance::new(&net_config.port_mappings)?;
        let socket_path = gvproxy.get_socket_path()?;

        tracing::info!(
            socket_path = ?socket_path,
            "Network backend created"
        );

        // Create NetworkBackendEndpoint from socket path
        // Platform-specific connection type:
        // - macOS: UnixDgram with VFKit protocol
        // - Linux: UnixStream with Qemu protocol
        let connection_type = if cfg!(target_os = "macos") {
            ConnectionType::UnixDgram
        } else {
            ConnectionType::UnixStream
        };

        // Use GUEST_MAC constant - must match DHCP static lease in gvproxy config
        use boxlite::net::constants::GUEST_MAC;

        config.network_backend_endpoint = Some(NetworkBackendEndpoint::UnixSocket {
            path: socket_path,
            connection_type,
            mac_address: GUEST_MAC,
        });

        // Leak the gvproxy instance to keep it alive for VM lifetime.
        // This is intentional - the VM needs networking for its entire life,
        // and OS cleanup handles resources when process exits.
        let _gvproxy_leaked = Box::leak(Box::new(gvproxy));
        tracing::debug!("Leaked gvproxy instance for VM lifetime");
    }

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
