//! ShimController - Universal process management for all Box engines.

use std::{
    path::PathBuf,
    process::Child,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::{
    management::BoxID,
    portal::GuestSession,
    vmm::{InstanceSpec, VmmController, VmmKind, VmmMetrics},
};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::{log_stream::LogStreamHandler, spawn::spawn_subprocess};

/// Universal subprocess-based Box controller.
///
/// This controller handles process management for all Box engines by spawning
/// the `boxlite-shim` binary in a subprocess. The subprocess isolation
/// ensures that Box process takeover doesn't affect the host application.
///
/// **Design**: Pure subprocess controller with no runtime dependencies.
/// State management (BoxManager updates) is handled by the caller.
/// Maintains a `System` instance for stateful CPU metrics tracking.
pub struct ShimController {
    binary_path: PathBuf,
    engine_type: VmmKind,
    process: Option<Child>,
    box_id: BoxID,
    log_handler: Option<LogStreamHandler>,
    /// Shared System instance for CPU metrics calculation across calls.
    /// CPU usage requires comparing snapshots over time, so we must reuse the same System.
    metrics_sys: Mutex<sysinfo::System>,
    /// Instant when controller was created (for lifecycle duration tracking).
    created_at: Instant,
    /// Guest boot duration in milliseconds (subprocess spawn to guest ready).
    /// Set after successful start().
    guest_boot_duration_ms: Option<u128>,
}

impl ShimController {
    /// Create a new ShimController.
    ///
    /// # Arguments
    /// * `binary_path` - Path to the boxlite-shim binary
    /// * `engine_type` - Type of Box engine to use (libkrun, firecracker, etc.)
    /// * `box_id` - Unique identifier for this box
    ///
    /// # Returns
    /// * `Ok(ShimController)` - Successfully created controller
    /// * `Err(...)` - Failed to create controller
    pub fn new(binary_path: PathBuf, engine_type: VmmKind, box_id: BoxID) -> BoxliteResult<Self> {
        // Verify that the Box runner binary exists
        if !binary_path.exists() {
            return Err(BoxliteError::Engine(format!(
                "Box runner binary not found: {}",
                binary_path.display()
            )));
        }

        Ok(Self {
            binary_path,
            engine_type,
            process: None,
            box_id,
            log_handler: None,
            metrics_sys: Mutex::new(sysinfo::System::new()),
            created_at: Instant::now(),
            guest_boot_duration_ms: None,
        })
    }

    /// Get the process ID if the subprocess is running.
    pub fn pid(&self) -> Option<u32> {
        self.process.as_ref().map(|p| p.id())
    }

    /// Wait for the guest to signal readiness via the ready notification socket.
    ///
    /// The host listens on the ready socket, and the guest connects when its
    /// gRPC server is ready to serve. This is more efficient than polling.
    async fn wait_for_guest_ready(
        &mut self,
        ready_transport: &boxlite_shared::Transport,
    ) -> BoxliteResult<()> {
        let ready_socket_path = match ready_transport {
            boxlite_shared::Transport::Unix { socket_path } => socket_path,
            _ => {
                return Err(BoxliteError::Engine(
                    "ready transport must be Unix socket".into(),
                ));
            }
        };

        // Remove stale socket if exists
        if ready_socket_path.exists() {
            let _ = std::fs::remove_file(ready_socket_path);
        }

        // Create listener for ready notification
        let listener = tokio::net::UnixListener::bind(ready_socket_path).map_err(|e| {
            let err_msg = format!(
                "Failed to bind ready socket {}: {}",
                ready_socket_path.display(),
                e
            );
            tracing::error!(box_id = %self.box_id, "{}", err_msg);
            BoxliteError::Engine(err_msg)
        })?;
        tracing::debug!(
            socket = %ready_socket_path.display(),
            "Listening for guest ready notification"
        );

        // Wait for guest connection with timeout
        let timeout = Duration::from_secs(30);
        let accept_result = tokio::time::timeout(timeout, listener.accept()).await;

        match accept_result {
            Ok(Ok((_stream, _addr))) => {
                // Guest connected - it's ready to serve
                tracing::debug!("Guest signaled ready via socket connection");
                Ok(())
            }
            Ok(Err(e)) => {
                let err_msg = format!("Ready socket accept failed: {}", e);
                tracing::error!(box_id = %self.box_id, "{}", err_msg);
                Err(BoxliteError::Engine(err_msg))
            }
            Err(_) => {
                // Check if process exited
                if let Some(ref mut process) = self.process
                    && let Ok(Some(status)) = process.try_wait()
                {
                    let err_msg = format!(
                        "Box process exited prematurely with status: {:?}",
                        status.code()
                    );
                    tracing::error!(box_id = %self.box_id, "{}", err_msg);
                    return Err(BoxliteError::Engine(err_msg));
                }
                let err_msg = format!("Guest failed to signal ready within {}s", timeout.as_secs());
                tracing::error!(box_id = %self.box_id, "{}", err_msg);
                Err(BoxliteError::Engine(err_msg))
            }
        }
    }

    /// Get the guest boot duration in milliseconds.
    ///
    /// Returns the time from subprocess spawn to guest agent ready.
    /// Returns None if start() hasn't been called yet or failed.
    pub fn guest_boot_duration_ms(&self) -> Option<u128> {
        self.guest_boot_duration_ms
    }
}

#[async_trait::async_trait]
impl VmmController for ShimController {
    async fn start(&mut self, config: &InstanceSpec) -> BoxliteResult<GuestSession> {
        if self.process.is_some() {
            return Err(BoxliteError::Engine("Box is already running".into()));
        }

        tracing::debug!(
            "Preparing config: entrypoint.executable={}, entrypoint.args={:?}",
            config.guest_entrypoint.executable,
            config.guest_entrypoint.args
        );

        // Prepare environment with RUST_LOG if present
        // Note: We can't clone BoxConfig anymore (contains non-clonable network_backend)
        // But serde(skip) on network_backend means it won't be serialized anyway
        let mut env = config.guest_entrypoint.env.clone();
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            env.push(("RUST_LOG".to_string(), rust_log.clone()));
        }

        // Create a temporary struct for serialization with modified env
        // This avoids cloning the config which now contains non-clonable NetworkBackend
        let serializable_config = InstanceSpec {
            cpus: config.cpus,
            memory_mib: config.memory_mib,
            volumes: config.volumes.clone(),
            disks: config.disks.clone(),
            guest_entrypoint: config.guest_entrypoint.clone(),
            transport: config.transport.clone(),
            ready_transport: config.ready_transport.clone(),
            init_rootfs: config.init_rootfs.clone(),
            network_backend_endpoint: config.network_backend_endpoint.clone(), // Pass connection info to subprocess
            home_dir: config.home_dir.clone(),
            console_output: config.console_output.clone(),
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
        let mut child = spawn_subprocess(&self.binary_path, self.engine_type, &config_json)?;
        // spawn_duration: time to create Box subprocess
        let shim_spawn_duration = shim_spawn_start.elapsed();

        // Extract pipes and create log stream handler
        let stdout = child.stdout.take().ok_or_else(|| {
            BoxliteError::Engine("Failed to capture subprocess stdout (pipe not available)".into())
        })?;

        let stderr = child.stderr.take().ok_or_else(|| {
            BoxliteError::Engine("Failed to capture subprocess stderr (pipe not available)".into())
        })?;

        let log_handler = LogStreamHandler::new(stdout, stderr).inspect_err(|_| {
            let _ = child.kill();
        })?;

        let pid = child.id();
        tracing::info!(
            box_id = %self.box_id,
            pid = pid,
            shim_spawn_duration_ms = shim_spawn_duration.as_millis(),
            "boxlite-shim subprocess spawned"
        );

        // Store process and log handler
        self.process = Some(child);
        self.log_handler = Some(log_handler);

        // Wait for guest to signal readiness via ready socket
        tracing::debug!("Waiting for guest to be ready");
        let boot_start = Instant::now();
        self.wait_for_guest_ready(&config.ready_transport).await?;

        // Create guest session for gRPC communication
        let guest = GuestSession::new(config.transport.clone());

        // Calculate durations:
        // - guest_boot_duration: time from subprocess spawn to guest ready
        // - total_startup: time from controller creation to guest ready
        let guest_boot_duration = boot_start.elapsed();
        let total_startup = self.created_at.elapsed();

        // Store guest boot duration for metrics
        self.guest_boot_duration_ms = Some(guest_boot_duration.as_millis());

        // Get initial memory snapshot
        let initial_metrics = self.metrics()?;

        tracing::info!(
            box_id = %self.box_id,
            guest_boot_duration_ms = guest_boot_duration.as_millis(),
            total_startup_ms = total_startup.as_millis(),
            initial_memory_mib = initial_metrics.memory_bytes.map(|b| b / 1024 / 1024),
            "Guest is ready"
        );

        Ok(guest)
    }

    fn stop(&mut self) -> BoxliteResult<()> {
        // Kill process first (this closes pipes, causing reader threads to exit)
        if let Some(mut process) = self.process.take() {
            let _ = process.kill();
            let _ = process.wait();
        }

        // Wait for log handler threads to finish gracefully
        if let Some(handler) = self.log_handler.take() {
            handler.shutdown()?;
        }

        Ok(())
    }

    fn metrics(&self) -> BoxliteResult<VmmMetrics> {
        use sysinfo::Pid;

        // Check if we have a running process
        if let Some(ref process) = self.process {
            let pid = Pid::from_u32(process.id());

            // Use the shared System instance for stateful CPU tracking
            // CPU calculation: (current_cpu_time - previous_cpu_time) / elapsed_time
            // This requires keeping System state between calls
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
        }

        // Process not found or not running - return empty metrics
        Ok(VmmMetrics::default())
    }

    fn is_running(&self) -> bool {
        // For ShimController, we consider the Box running if we have a process handle
        // In the current implementation, we wait for the subprocess to complete in start()
        // so this will typically return false after start() completes
        self.process.is_some()
    }
}
