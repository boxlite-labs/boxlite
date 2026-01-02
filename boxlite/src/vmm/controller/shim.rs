//! ShimController and ShimHandler - Universal process management for all Box engines.

use std::{path::PathBuf, process::Child, sync::Mutex, time::Instant};

use crate::{
    BoxID,
    vmm::{InstanceSpec, VmmKind},
};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::{VmmController, VmmHandler as VmmHandlerTrait, VmmMetrics, spawn::spawn_subprocess};

// ============================================================================
// SHIM HANDLER - Runtime operations on running VM
// ============================================================================

/// Runtime handler for a running VM subprocess.
///
/// Provides lifecycle operations (stop, metrics, status) for a VM identified by PID.
/// Works for both spawned VMs and reconnected VMs (same operations).
pub struct ShimHandler {
    pid: u32,
    #[allow(dead_code)]
    box_id: BoxID,
    /// Child process handle for proper lifecycle management.
    /// When we spawn the process, we keep the Child to properly wait() on stop.
    /// When we attach to an existing process, this is None.
    process: Option<Child>,
    /// Shared System instance for CPU metrics calculation across calls.
    /// CPU usage requires comparing snapshots over time, so we must reuse the same System.
    metrics_sys: Mutex<sysinfo::System>,
}

impl ShimHandler {
    /// Create a handler for a spawned VM with process ownership.
    ///
    /// This constructor takes ownership of the Child process handle for proper
    /// lifecycle management (clean shutdown with wait()).
    ///
    /// # Arguments
    /// * `process` - The spawned subprocess (Child handle)
    /// * `box_id` - Box identifier (for logging)
    pub fn from_child(process: Child, box_id: BoxID) -> Self {
        let pid = process.id();
        Self {
            pid,
            box_id,
            process: Some(process),
            metrics_sys: Mutex::new(sysinfo::System::new()),
        }
    }

    /// Create a handler for an existing VM (attach mode).
    ///
    /// Used when reconnecting to a running box. We don't have a Child handle,
    /// so we manage the process by PID only.
    ///
    /// # Arguments
    /// * `pid` - Process ID of the running VM
    /// * `box_id` - Box identifier (for logging)
    pub fn from_pid(pid: u32, box_id: BoxID) -> Self {
        Self {
            pid,
            box_id,
            process: None,
            metrics_sys: Mutex::new(sysinfo::System::new()),
        }
    }
}

impl VmmHandlerTrait for ShimHandler {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn stop(&mut self) -> BoxliteResult<()> {
        // Kill process - prefer Child::kill() if we have the handle
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait(); // Reap zombie process
        } else {
            // Attached mode: kill using libc::kill since we don't have a Child handle
            unsafe {
                libc::kill(self.pid as i32, libc::SIGKILL);
            }
        }

        Ok(())
    }

    fn metrics(&self) -> BoxliteResult<VmmMetrics> {
        use sysinfo::Pid;

        let pid = Pid::from_u32(self.pid);

        // Use the shared System instance for stateful CPU tracking
        let mut sys = self
            .metrics_sys
            .lock()
            .map_err(|e| BoxliteError::Internal(format!("metrics_sys lock poisoned: {}", e)))?;

        // Refresh process info - this updates the internal state for delta calculation
        sys.refresh_process(pid);

        // Try to get process information
        if let Some(proc_info) = sys.process(pid) {
            return Ok(VmmMetrics {
                cpu_percent: Some(proc_info.cpu_usage()),
                memory_bytes: Some(proc_info.memory()),
                disk_bytes: None, // Not available from process-level APIs
            });
        }

        // Process not found or not running - return empty metrics
        Ok(VmmMetrics::default())
    }

    fn is_running(&self) -> bool {
        crate::util::is_process_alive(self.pid)
    }
}

// ============================================================================
// SHIM CONTROLLER - Spawning operations
// ============================================================================

/// Controller for spawning VM subprocesses.
///
/// Spawns the `boxlite-shim` binary in a subprocess and returns a ShimHandler
/// for runtime operations. The subprocess isolation ensures that VM process
/// takeover doesn't affect the host application.
pub struct ShimController {
    binary_path: PathBuf,
    engine_type: VmmKind,
    box_id: BoxID,
}

impl ShimController {
    /// Create a new ShimController.
    ///
    /// # Arguments
    /// * `binary_path` - Path to the boxlite-shim binary
    /// * `engine_type` - Type of VM engine to use (libkrun, firecracker, etc.)
    /// * `box_id` - Unique identifier for this box
    ///
    /// # Returns
    /// * `Ok(ShimController)` - Successfully created controller
    /// * `Err(...)` - Failed to create controller (e.g., binary not found)
    pub fn new(binary_path: PathBuf, engine_type: VmmKind, box_id: BoxID) -> BoxliteResult<Self> {
        // Verify that the shim binary exists
        if !binary_path.exists() {
            return Err(BoxliteError::Engine(format!(
                "Box runner binary not found: {}",
                binary_path.display()
            )));
        }

        Ok(Self {
            binary_path,
            engine_type,
            box_id,
        })
    }
}

#[async_trait::async_trait]
impl VmmController for ShimController {
    async fn start(&mut self, config: &InstanceSpec) -> BoxliteResult<Box<dyn VmmHandlerTrait>> {
        tracing::debug!(
            "Preparing config: entrypoint.executable={}, entrypoint.args={:?}",
            config.guest_entrypoint.executable,
            config.guest_entrypoint.args
        );

        // Prepare environment with RUST_LOG if present
        // Note: We clone the config components needed for subprocess serialization
        let mut env = config.guest_entrypoint.env.clone();
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            env.push(("RUST_LOG".to_string(), rust_log.clone()));
        }

        // Create a temporary struct for serialization with modified env
        // This avoids cloning the config which now contains non-clonable NetworkBackend
        let mut guest_entrypoint = config.guest_entrypoint.clone();
        guest_entrypoint.env = env; // Use the modified env with RUST_LOG

        let serializable_config = InstanceSpec {
            cpus: config.cpus,
            memory_mib: config.memory_mib,
            fs_shares: config.fs_shares.clone(),
            block_devices: config.block_devices.clone(),
            guest_entrypoint,
            transport: config.transport.clone(),
            ready_transport: config.ready_transport.clone(),
            guest_rootfs: config.guest_rootfs.clone(),
            network_config: config.network_config.clone(), // Pass port mappings to subprocess (shim creates gvproxy)
            network_backend_endpoint: None, // Will be populated by shim (not serialized)
            home_dir: config.home_dir.clone(),
            console_output: config.console_output.clone(),
            detach: config.detach,
            parent_pid: config.parent_pid,
        };

        // Serialize the config for passing to subprocess
        let config_json = serde_json::to_string(&serializable_config)
            .map_err(|e| BoxliteError::Engine(format!("Failed to serialize config: {}", e)))?;

        // Clean up stale socket file if it exists (defense in depth)
        // Only relevant for Unix sockets
        if let boxlite_shared::Transport::Unix { socket_path } = &config.transport
            && socket_path.exists()
        {
            tracing::warn!("Removing stale Unix socket: {}", socket_path.display());
            let _ = std::fs::remove_file(socket_path);
        }

        // Spawn Box subprocess with piped stdio
        tracing::info!(
            engine = ?self.engine_type,
            transport = ?config.transport,
            "Starting Box subprocess"
        );
        tracing::debug!(binary = %self.binary_path.display(), "Box runner binary");
        tracing::trace!(config = %config_json, "Box configuration");

        // Measure subprocess spawn time
        let shim_spawn_start = Instant::now();
        let child = spawn_subprocess(&self.binary_path, self.engine_type, &config_json)?;
        // spawn_duration: time to create Box subprocess
        let shim_spawn_duration = shim_spawn_start.elapsed();

        let pid = child.id();
        tracing::info!(
            box_id = %self.box_id,
            pid = pid,
            shim_spawn_duration_ms = shim_spawn_duration.as_millis(),
            "boxlite-shim subprocess spawned"
        );

        // Note: We don't wait for guest readiness here anymore.
        // GuestConnectTask handles waiting for guest readiness,
        // which allows reusing that task across spawn/restart/reconnect.

        // Create handler for the running VM
        // Note: stdio is null (no pipes), so no LogStreamHandler needed
        let handler = ShimHandler::from_child(child, self.box_id.clone());

        tracing::info!(
            box_id = %self.box_id,
            "VM subprocess started successfully"
        );

        // Note: Child is dropped here, but process continues running
        // Handler manages it by PID
        Ok(Box::new(handler))
    }
}
