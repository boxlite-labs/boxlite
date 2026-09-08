//! ShimController and ShimHandler - Universal process management for all Box engines.

use std::{path::PathBuf, process::Child, sync::Mutex, time::Instant};

use crate::{
    BoxID,
    runtime::layout::BoxFilesystemLayout,
    vmm::{InstanceSpec, VmmKind},
};

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::watchdog;
use super::{
    VmmController, VmmHandler as VmmHandlerTrait, VmmMetrics,
    spawn::{ShimSpawner, SpawnedShim},
};

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
    /// Watchdog keepalive. Dropping closes the pipe write end, delivering
    /// POLLHUP to the shim and triggering graceful shutdown.
    /// Defense-in-depth: even if `stop()` is never called, dropping the
    /// handler closes this, triggering shim cleanup automatically.
    #[allow(dead_code)]
    keepalive: Option<watchdog::Keepalive>,
    /// `/proc` start-time of `pid`, captured when this handler was built.
    ///
    /// The reap sweep enumerates the process tree below `self.pid`, so a pid
    /// recycled to an unrelated process would have us signal a stranger's
    /// children. Attach already rejects a reused pid when it reads the PID
    /// file, but that check ages; this is the same fingerprint, re-read at the
    /// moment it is acted on. `None` when the OS reading is unavailable, which
    /// keeps the sweep's previous behaviour rather than skipping it blindly.
    start_time: Option<u64>,
    /// Shared System instance for CPU metrics calculation across calls.
    /// CPU usage requires comparing snapshots over time, so we must reuse the same System.
    metrics_sys: Mutex<sysinfo::System>,
}

impl ShimHandler {
    /// Create a handler from a spawned shim.
    ///
    /// Takes ownership of the `SpawnedShim` (child process + keepalive) for
    /// proper lifecycle management. The keepalive keeps the watchdog pipe
    /// alive; dropping it triggers shim shutdown.
    pub fn from_spawned(spawned: SpawnedShim, box_id: BoxID) -> Self {
        let pid = spawned.child.id();
        Self {
            pid,
            box_id,
            start_time: crate::util::process_start_time(pid),
            process: Some(spawned.child),
            keepalive: spawned.keepalive,
            metrics_sys: Mutex::new(sysinfo::System::new()),
        }
    }

    /// Create a handler for an existing VM (attach mode).
    ///
    /// Used when reconnecting to a running box. We don't have a Child handle
    /// or keepalive, so we manage the process by PID only.
    ///
    /// # Arguments
    /// * `pid` - Process ID of the running VM
    /// * `box_id` - Box identifier (for logging)
    pub fn from_pid(pid: u32, box_id: BoxID) -> Self {
        Self {
            pid,
            box_id,
            start_time: crate::util::process_start_time(pid),
            process: None,
            keepalive: None,
            metrics_sys: Mutex::new(sysinfo::System::new()),
        }
    }

    /// Whether `self.pid` is still the process this handler was built for.
    ///
    /// Compares the live `/proc` start-time against the one captured at
    /// construction. A dead pid reads `None` and fails the comparison, which is
    /// correct: there is no tree left to sweep. Returns `true` when no
    /// fingerprint was captured — identity cannot be disproven, so the sweep
    /// keeps the behaviour it had before this guard existed.
    fn pid_identity_holds(&self) -> bool {
        match self.start_time {
            Some(captured) => crate::util::process_start_time(self.pid) == Some(captured),
            None => true,
        }
    }

    /// Whether the process this handler was built for is still running.
    ///
    /// Not the same question as bare liveness, and teardown must ask this one: a
    /// pid recycled since construction is alive, yet answering "yes" for it lets
    /// a wait loop run its full course and then escalate onto a stranger. A
    /// recycled pid has to read as gone.
    ///
    /// Collapses to plain liveness for a handler with no fingerprint, which is
    /// the most that can be said about one.
    fn recorded_process_is_running(&self) -> bool {
        crate::util::is_process_alive(self.pid) && self.pid_identity_holds()
    }

    /// Graceful shutdown of the recorded process: SIGTERM, wait, then SIGKILL.
    ///
    /// Signals only `self.pid` (the outer launcher). The full process-tree
    /// sweep happens in `stop()` after this returns — see the comment there for
    /// why the order matters (libkrun must flush before the hard cgroup kill).
    fn graceful_stop(&mut self) -> BoxliteResult<()> {
        // Graceful shutdown: SIGTERM first, wait, then SIGKILL if needed.
        // This gives libkrun time to flush its virtio-blk buffers to disk,
        // preventing qcow2 corruption.
        const GRACEFUL_SHUTDOWN_TIMEOUT_MS: u64 = 2000;

        if let Some(mut process) = self.process.take() {
            // Step 1: Send SIGTERM for graceful shutdown
            let pid = process.id();
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }

            // Step 2: Wait with timeout for process to exit
            let start = std::time::Instant::now();
            loop {
                match process.try_wait() {
                    Ok(Some(_)) => {
                        // Process exited gracefully
                        return Ok(());
                    }
                    Ok(None) => {
                        // Still running, check timeout
                        if start.elapsed().as_millis() > GRACEFUL_SHUTDOWN_TIMEOUT_MS as u128 {
                            // Timeout - force kill
                            let _ = process.kill();
                            let _ = process.wait();
                            return Ok(());
                        }
                        // Brief sleep before checking again
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Err(_) => {
                        // Error checking status - try to kill anyway
                        let _ = process.kill();
                        let _ = process.wait();
                        return Ok(());
                    }
                }
            }
        } else {
            // Attached mode: use SIGTERM then SIGKILL with polling
            // We don't have a Child handle, so we use waitpid/kill directly.
            //
            // Without a `Child` nothing pins the pid, so it is the one shape
            // that can have been recycled since this handler was built. Signal
            // it only while its start-time still matches: otherwise these two
            // kills land on whatever process inherited the number.
            if !self.pid_identity_holds() {
                tracing::warn!(
                    box_id = %self.box_id,
                    pid = self.pid,
                    "Shim pid no longer matches its recorded start-time; \
                     not signalling it"
                );
                return Ok(());
            }

            unsafe {
                libc::kill(self.pid as i32, libc::SIGTERM);
            }

            // Poll for exit with timeout
            let start = std::time::Instant::now();
            loop {
                let mut status: i32 = 0;
                let result = unsafe { libc::waitpid(self.pid as i32, &mut status, libc::WNOHANG) };

                if result > 0 {
                    // Process exited gracefully (we reaped it)
                    return Ok(());
                }
                if result < 0 {
                    // Error - process may not be our child (common in attached
                    // mode). Fall back to asking whether *our* process is still
                    // there. Liveness alone would not do: the pid can be
                    // recycled while we poll, and a bare "it exists" keeps the
                    // loop running until the timeout below fires on a stranger.
                    if !self.recorded_process_is_running() {
                        return Ok(()); // Already dead, or no longer ours
                    }
                }
                // result == 0 means still running

                if start.elapsed().as_millis() > GRACEFUL_SHUTDOWN_TIMEOUT_MS as u128 {
                    // Timeout - force kill, but only while the pid is still the
                    // process we set out to stop. Up to a full poll interval has
                    // passed since the check above, and the entry guard is by now
                    // seconds stale; re-reading here narrows the window in which
                    // a recycled pid can take this SIGKILL down to a syscall,
                    // matching what `jailer::signal_live` does per descendant.
                    if self.recorded_process_is_running() {
                        unsafe {
                            libc::kill(self.pid as i32, libc::SIGKILL);
                        }
                    } else {
                        tracing::warn!(
                            box_id = %self.box_id,
                            pid = self.pid,
                            "Shim pid stopped matching its recorded start-time during \
                             shutdown; not force-killing it"
                        );
                    }
                    return Ok(());
                }

                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        #[allow(unreachable_code)]
        Ok(())
    }
}

impl VmmHandlerTrait for ShimHandler {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn stop(&mut self) -> BoxliteResult<()> {
        // `graceful_stop` only signals the recorded pid — the outer bwrap
        // launcher — and a detached box's inner pid-ns tree (inner bwrap + shim +
        // VM) outlives it, since #851 stopped applying `--die-with-parent` to
        // detached boxes. Snapshot that tree *before* shutdown: once the launcher
        // exits its children are reparented and can no longer be found from
        // `self.pid`.
        //
        // Only when `self.pid` is still the process this handler was built for:
        // the walk trusts that pid as the tree's root, so a pid recycled since
        // then would hand us a stranger's children to SIGTERM and SIGKILL.
        // `graceful_stop` carries the same guard for the root itself, leaving
        // `reap_box` — keyed by box id, not pid — as the only thing that still
        // runs, which is exactly what is safe to run against a stale pid.
        let box_tree = if self.pid_identity_holds() {
            crate::jailer::collect_descendants(self.pid)
        } else {
            tracing::warn!(
                box_id = %self.box_id,
                pid = self.pid,
                "Shim pid no longer matches its recorded start-time (exited, or \
                 reused by another process); skipping the process-tree sweep"
            );
            Vec::new()
        };

        // Stop the recorded launcher (SIGTERM, wait, SIGKILL).
        let result = self.graceful_stop();

        // Give the inner tree a graceful SIGTERM + bounded wait before any hard
        // kill: for a detached box the shim is in its own session, so the
        // launcher's shutdown above never reached it — this is the shim's only
        // chance to flush libkrun's virtio-blk buffers. Reaping mid-flush risks
        // qcow2 corruption. Runs before `reap_box` so the cgroup path flushes too.
        //
        // Sized from what the shim is allowed to take, not from a guess: its
        // SIGTERM handler gives `Guest.Shutdown` `GUEST_SHUTDOWN_TIMEOUT_SECS`
        // (3s, `shim/src/main.rs:292`) and only re-raises SIGTERM once that
        // returns, so anything shorter can expire mid-flush — measured 2105,
        // 2111 and 2131 ms on a detached box, all of which a 2s grace missed.
        // The extra second covers the re-raise and process exit.
        //
        // This comment is the only thing tying the two values together; they
        // live in different crates, so raising the shim's timeout without
        // raising this one silently reintroduces the truncation.
        //
        // Scope: the measurements above are from a detached box, which is the
        // case this sweep exists for. A foreground box carries
        // `--die-with-parent` (`jailer/sandbox/bwrap.rs:78`), so its shim dies
        // with the launcher inside `graceful_stop` and never reaches this wait —
        // whether that path wants a flush window of its own is untouched here.
        const REAP_GRACE: std::time::Duration = std::time::Duration::from_millis(4000);
        crate::jailer::terminate_and_wait(&box_tree, REAP_GRACE);

        // Reap survivors. `reap_box` uses cgroup.kill (atomic, fork-safe) when the
        // box owns a usable cgroup; rootless without cgroup delegation (WSL2 / CI
        // / no-systemd, where the box cgroup can't be created or populated) it
        // no-ops, so also SIGKILL the captured tree — killing the inner pid-ns
        // init reaps the namespace. Both are idempotent, and `reap_pids` skips any
        // pid already gone or recycled to an unrelated process.
        crate::jailer::reap_box(&self.box_id);
        crate::jailer::reap_pids(&box_tree);
        result
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

/// Emit the post-spawn TRACE event with the serialized `InstanceSpec`'s
/// secrets stripped out.
///
/// The serialized `InstanceSpec` (`config_json`) carries
/// `NetworkBackendConfig.secrets` (user-provided secret values) and, when a
/// MITM proxy is configured, `ca_key_pem` — the PKCS8 CA private key. The
/// config is deliberately piped via stdin rather than CLI args so those bytes
/// stay out of `/proc/<pid>/cmdline` (see `spawn.rs:97-99`); serializing them
/// into a TRACE log would defeat that mitigation.
///
/// This helper is the single audited site for that trace event so that
/// `redacted_box_config_trace_does_not_emit_config_json` can pin the
/// behavior with a real subscriber capture rather than relying on
/// source-grep heuristics.
fn emit_redacted_box_config_trace(engine: VmmKind, box_id: &str, config_json: &str) {
    tracing::trace!(
        engine = ?engine,
        box_id = %box_id,
        json_bytes = config_json.len(),
        "Box configuration prepared (raw config not logged; contains secrets)"
    );
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
    /// Box options (includes security and volumes for jailer isolation)
    options: crate::runtime::options::BoxOptions,
    /// Box filesystem layout (provides paths for stderr, sockets, etc.)
    layout: BoxFilesystemLayout,
}

impl ShimController {
    /// Create a new ShimController.
    ///
    /// # Arguments
    /// * `binary_path` - Path to the boxlite-shim binary
    /// * `engine_type` - Type of VM engine to use (libkrun, firecracker, etc.)
    /// * `box_id` - Unique identifier for this box
    /// * `options` - Box options (includes security and volumes)
    /// * `layout` - Box filesystem layout
    ///
    /// # Returns
    /// * `Ok(ShimController)` - Successfully created controller
    /// * `Err(...)` - Failed to create controller (e.g., binary not found)
    pub fn new(
        binary_path: PathBuf,
        engine_type: VmmKind,
        box_id: BoxID,
        options: crate::runtime::options::BoxOptions,
        layout: BoxFilesystemLayout,
    ) -> BoxliteResult<Self> {
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
            options,
            layout,
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
            engine: self.engine_type,
            // Box identification and security (from ShimController)
            box_id: self.box_id.to_string(),
            security: self.options.advanced.security.clone(),
            nested_virtualization: config.nested_virtualization,
            // VM configuration
            cpus: config.cpus,
            memory_mib: config.memory_mib,
            kernel: config.kernel.clone(),
            fs_shares: config.fs_shares.clone(),
            block_devices: config.block_devices.clone(),
            guest_entrypoint,
            transport: config.transport.clone(),
            ready_transport: config.ready_transport.clone(),
            guest_rootfs: config.guest_rootfs.clone(),
            network_backend_spec: config.network_backend_spec.clone(), // provisioning spec passed to the shim (stands up gvproxy)
            network_backend_endpoint: None, // Will be populated by shim (not serialized)
            disable_network: config.disable_network,
            home_dir: config.home_dir.clone(),
            console_output: config.console_output.clone(),
            exit_file: config.exit_file.clone(),
            detach: config.detach,
        };

        // Serialize the config for passing to subprocess
        let config_json = serde_json::to_string(&serializable_config)
            .map_err(|e| BoxliteError::Engine(format!("Failed to serialize config: {}", e)))?;

        // Clean up stale socket file if it exists (defense in depth)
        // Only relevant for Unix sockets
        if let boxlite_shared::BoxTransport::Unix { socket_path } = &config.transport
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
        emit_redacted_box_config_trace(self.engine_type, self.box_id.as_str(), &config_json);

        // Measure subprocess spawn time
        let shim_spawn_start = Instant::now();
        let spawner = ShimSpawner::new(
            &self.binary_path,
            &self.layout,
            self.box_id.as_str(),
            &self.options,
        )
        .with_nested_virtualization(config.nested_virtualization);
        let spawned = spawner.spawn(&config_json, config.detach)?;
        // spawn_duration: time to create Box subprocess
        let shim_spawn_duration = shim_spawn_start.elapsed();

        let pid = spawned.child.id();
        tracing::info!(
            box_id = %self.box_id,
            pid = pid,
            shim_spawn_duration_ms = shim_spawn_duration.as_millis(),
            "boxlite-shim subprocess spawned"
        );

        // Note: We don't wait for guest readiness here anymore.
        // GuestConnectTask handles waiting for guest readiness,
        // which allows reusing that task across spawn/restart/reconnect.

        // Create handler from spawned shim (takes ownership of child + keepalive)
        let handler = ShimHandler::from_spawned(spawned, self.box_id.clone());

        tracing::info!(
            box_id = %self.box_id,
            "VM subprocess started successfully"
        );

        // Note: Child is dropped here, but process continues running
        // Handler manages it by PID
        Ok(Box::new(handler))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Captures `tracing` output into a shared byte buffer so a test can
    /// assert what was (and wasn't) written by an event emitted within
    /// `tracing::subscriber::with_default`.
    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Behavioral regression for the leak fixed in this commit: feed a
    /// `config_json` whose body contains sentinel CA-key / secret bytes
    /// into the helper that is the *only* site allowed to emit the
    /// post-spawn TRACE event, capture every byte the subscriber writes,
    /// and assert the sentinels never appear — while the redacted fields
    /// (`box_id`, `json_bytes`) do. If any future change reintroduces
    /// `config = %config_json` (or any other form that lets the raw bytes
    /// reach the subscriber), the sentinel assertion fires.
    #[test]
    fn redacted_box_config_trace_does_not_emit_config_json() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(BufWriter(buf.clone()))
            .with_ansi(false)
            .finish();

        let key_sentinel = "PKCS8_PRIVATE_KEY_SENTINEL_DO_NOT_LEAK";
        let secret_sentinel = "USER_SECRET_VALUE_SENTINEL_DO_NOT_LEAK";
        let config_json = format!(
            r#"{{"secrets":[{{"name":"db","value":"{}"}}],"ca_key_pem":"-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----"}}"#,
            secret_sentinel, key_sentinel
        );
        let expected_len = config_json.len();

        tracing::subscriber::with_default(subscriber, || {
            emit_redacted_box_config_trace(VmmKind::Libkrun, "test-box-id", &config_json);
        });

        let output = String::from_utf8(buf.lock().unwrap().clone()).expect("utf8 trace output");

        assert!(
            !output.contains(key_sentinel),
            "CA private key sentinel leaked into trace output: {output}"
        );
        assert!(
            !output.contains(secret_sentinel),
            "user secret sentinel leaked into trace output: {output}"
        );
        assert!(
            output.contains("test-box-id"),
            "box_id (non-sensitive) should appear in trace output: {output}"
        );
        assert!(
            output.contains(&format!("json_bytes={expected_len}")),
            "json_bytes redacted summary should appear: {output}"
        );
    }

    /// The reap sweep's root guard. A live pid whose recorded start-time still
    /// matches is the box's own shim and its tree may be swept; the same pid
    /// carrying a stale fingerprint is a recycled number whose children belong
    /// to someone else. The fingerprints come from the OS via
    /// `process_start_time`, not from the test, so the matching case proves the
    /// comparison actually reads `/proc` rather than trivially agreeing.
    #[test]
    fn pid_identity_guard_rejects_a_recycled_pid() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        let live = ShimHandler::from_pid(pid, BoxID::parse("pidguardtest").expect("valid id"));
        assert!(
            live.start_time.is_some(),
            "the OS must report a start-time for a process we just spawned"
        );
        assert!(
            live.pid_identity_holds(),
            "a live pid with its own recorded start-time must pass the guard"
        );

        // Same pid, a fingerprint that cannot be its own: what a recycled pid
        // looks like from here.
        let recycled = ShimHandler {
            start_time: live.start_time.map(|t| t.wrapping_add(1)),
            ..ShimHandler::from_pid(pid, BoxID::parse("pidguardtest").expect("valid id"))
        };
        assert!(
            !recycled.pid_identity_holds(),
            "a start-time mismatch must fail the guard, so the sweep is skipped"
        );

        // No fingerprint: identity cannot be disproven, so the sweep keeps the
        // behaviour it had before the guard existed.
        let legacy = ShimHandler {
            start_time: None,
            ..ShimHandler::from_pid(pid, BoxID::parse("pidguardtest").expect("valid id"))
        };
        assert!(
            legacy.pid_identity_holds(),
            "an unfingerprinted handler must not have its sweep suppressed"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// End-to-end counterpart to the predicate test: a handler holding a stale
    /// fingerprint must not signal the process that now owns its pid. The
    /// predicate alone does not pin this — `stop()` reaches the pid twice, once
    /// through the sweep and once through `graceful_stop`, and guarding only
    /// the first still leaves the second to SIGTERM and SIGKILL a stranger.
    #[test]
    fn stop_does_not_signal_a_process_that_reused_the_pid() {
        let mut bystander = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = bystander.id();

        // Attached shape (`process: None`) carrying a fingerprint that cannot
        // be this pid's — what a recycled pid looks like at stop() time.
        let mut handler = ShimHandler {
            start_time: crate::util::process_start_time(pid).map(|t| t.wrapping_add(1)),
            ..ShimHandler::from_pid(pid, BoxID::parse("pidguardtest").expect("valid id"))
        };
        assert!(
            handler.start_time.is_some(),
            "the OS must report a start-time for a process we just spawned"
        );

        let _ = handler.stop();

        // SIGTERM would have killed `sleep` outright, so survival is the
        // observable that separates guarded from unguarded.
        assert!(
            crate::util::is_process_alive(pid),
            "stop() signalled a process whose start-time did not match the handler's"
        );

        let _ = bystander.kill();
        let _ = bystander.wait();
    }

    /// Workspace-wide regression: the behavioral test above pins the helper
    /// itself. It does **not** pin the invariant that the helper is the
    /// *only* site emitting `config_json` to a `tracing::*` macro — a new
    /// `tracing::trace!(config = %config_json, ...)` added anywhere else in
    /// the workspace (this crate, the shim binary which reads the same
    /// JSON from stdin, etc.) would slip past it.
    ///
    /// `tracing`'s `%` (Display) and `?` (Debug) field-value sigils are
    /// unambiguous markers for "this expression's formatted output goes
    /// into the event". Forbid both `%config_json` and `?config_json`
    /// anywhere in production source under the workspace's `src/`. Test
    /// code (anything past the first `#[cfg(test)]` in each file) is
    /// exempt so this patrol can talk about the patterns it catches
    /// without tripping over itself.
    #[test]
    fn no_config_json_in_tracing_sigil_workspace_wide() {
        fn walk_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(_) => return,
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    walk_rs(&p, out);
                } else if p.extension().is_some_and(|e| e == "rs") {
                    out.push(p);
                }
            }
        }

        // CARGO_MANIFEST_DIR points at this crate (src/boxlite); the
        // workspace root is two levels up, and the workspace-level source
        // tree (which also includes src/shim, src/cli, …) lives at
        // <workspace-root>/src.
        let workspace_src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("src");
        let mut files = Vec::new();
        walk_rs(&workspace_src, &mut files);
        assert!(
            !files.is_empty(),
            "patrol found zero .rs files under {}",
            workspace_src.display()
        );

        let forbidden = ["%config_json", "?config_json"];
        let mut offenders = Vec::new();
        for path in &files {
            let src = match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Strip every `#[cfg(test)]` block (only the first one is
            // sufficient because tests sit at the bottom of a file by
            // convention; if a project ever puts a `#[cfg(test)]` in the
            // middle, the patrol still gates everything above the first
            // such marker).
            let production = match src.find("#[cfg(test)]") {
                Some(idx) => &src[..idx],
                None => &src,
            };
            for needle in &forbidden {
                if production.contains(needle) {
                    offenders.push(format!("{}: contains {needle:?}", path.display()));
                }
            }
        }

        assert!(
            offenders.is_empty(),
            "`config_json` reached a `tracing::*` field sigil in production code — \
             that string carries secrets and a PKCS8 CA private key. Route the \
             log through `emit_redacted_box_config_trace` instead.\n  {}",
            offenders.join("\n  ")
        );
    }

    /// Set on the re-exec that runs the pid-reuse scenario inside a rootless
    /// user+pid namespace. Absent means this is the outer invocation, whose
    /// only job is to build that namespace and report what happened inside.
    #[cfg(target_os = "linux")]
    const PID_REUSE_NS_MARKER: &str = "BOXLITE_TEST_PID_REUSE_IN_NS";

    /// The attached branch checks the pid's identity once, before the SIGTERM,
    /// then polls for up to `GRACEFUL_SHUTDOWN_TIMEOUT_MS` before force-killing.
    /// A pid recycled inside that window gets the timeout `SIGKILL` under the
    /// *previous* occupant's warrant — the guard at the top of the branch has
    /// long since passed, and the poll's `is_process_alive` cannot tell the two
    /// occupants apart.
    ///
    /// Staging that needs a pid reused on demand, which the host cannot do:
    /// `pid_max` is in the millions so no fork storm returns to a given number,
    /// and `/proc/sys/kernel/ns_last_pid` is not writable. Both constraints lift
    /// inside a rootless user+pid namespace, so this re-execs itself into one
    /// and drives the reuse directly.
    #[cfg(target_os = "linux")]
    #[test]
    fn attached_timeout_kill_skips_a_pid_recycled_during_the_grace_window() {
        if std::env::var_os(PID_REUSE_NS_MARKER).is_some() {
            assert_recycled_pid_survives_the_timeout_kill();
            return;
        }

        let exe = std::env::current_exe().expect("test binary path");
        let output = match std::process::Command::new("unshare")
            .args(["-Ur", "--pid", "--fork", "--mount-proc"])
            .arg(&exe)
            // Substring filter: this name matches exactly one test, which
            // avoids having to reconstruct the harness' module path.
            .arg("attached_timeout_kill_skips_a_pid_recycled_during_the_grace_window")
            .args(["--nocapture", "--test-threads=1"])
            .env(PID_REUSE_NS_MARKER, "1")
            .output()
        {
            Ok(output) => output,
            Err(e) => {
                eprintln!("skipping: cannot run `unshare`: {e}");
                return;
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // A host that forbids unprivileged user namespaces cannot stage pid
            // reuse at all; that is a missing capability, not a regression.
            if stderr.contains("unshare failed") || stderr.contains("Operation not permitted") {
                eprintln!("skipping: rootless user+pid namespaces unavailable: {stderr}");
                return;
            }
            panic!(
                "pid-reuse scenario failed inside the namespace\n--- stdout ---\n{}\n--- stderr ---\n{stderr}",
                String::from_utf8_lossy(&output.stdout),
            );
        }
    }

    /// Whether `pid` currently has SIGTERM set to `SIG_IGN`, read from the
    /// `SigIgn` mask in `/proc/<pid>/status` (bit `SIGTERM - 1`).
    #[cfg(target_os = "linux")]
    fn ignores_sigterm(pid: u32) -> bool {
        let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
            return false;
        };
        status
            .lines()
            .find_map(|line| line.strip_prefix("SigIgn:"))
            .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
            .is_some_and(|mask| mask & (1 << (libc::SIGTERM - 1)) != 0)
    }

    /// Body of the test above, running as pid 1 of a fresh pid namespace.
    #[cfg(target_os = "linux")]
    fn assert_recycled_pid_survives_the_timeout_kill() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        // The poll runs every 50ms, so there is a gap between the victim being
        // reaped and the bystander taking its number in which the loop can see
        // the pid empty and return before ever reaching the timeout kill. That
        // is a staging miss, not a pass — `elapsed` below detects it, and this
        // is how many times we retry.
        const ATTEMPTS: usize = 6;

        for attempt in 1..=ATTEMPTS {
            // The victim must not be *our* child: the attached branch only
            // reaches its timeout while `waitpid` keeps failing, and a child
            // would be reaped by the poll itself, returning early. A helper
            // shell owns it and reaps it on demand. The victim ignores SIGTERM
            // so the timeline belongs to this test rather than to the signal.
            let mut helper = Command::new("sh")
                .arg("-c")
                .arg("sh -c 'trap \"\" TERM; sleep 3600' & echo $!; wait; exec sleep 3600")
                .stdout(Stdio::piped())
                .spawn()
                .expect("spawn the victim's owner");
            let victim: u32 = {
                let mut line = String::new();
                BufReader::new(helper.stdout.take().expect("helper stdout"))
                    .read_line(&mut line)
                    .expect("read the victim's pid");
                line.trim().parse().expect("victim pid is a number")
            };
            // `$!` is reported at fork time, before the shell has run its
            // `trap`. Signalling into that gap kills the victim outright and the
            // poll returns long before the timeout, so wait for the disposition
            // to actually be installed.
            while !ignores_sigterm(victim) {
                std::thread::sleep(Duration::from_millis(5));
            }

            // Frees the victim's number mid-grace and puts a bystander on it.
            let recycler = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(600));
                unsafe { libc::kill(victim as i32, libc::SIGKILL) };
                // Spin rather than sleep: every millisecond the number stays
                // empty is a millisecond the poll can notice it.
                let deadline = Instant::now() + Duration::from_millis(500);
                while std::path::Path::new(&format!("/proc/{victim}")).exists() {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::hint::spin_loop();
                }
                std::fs::write("/proc/sys/kernel/ns_last_pid", (victim - 1).to_string()).ok()?;
                Command::new("sleep").arg("3600").spawn().ok()
            });

            let mut handler =
                ShimHandler::from_pid(victim, BoxID::parse("pidreusetest").expect("valid id"));
            assert!(
                handler.pid_identity_holds(),
                "the handler must start out owning its pid, or the branch under \
                 test returns at its entry guard instead of reaching the timeout"
            );

            let started = Instant::now();
            let _ = handler.graceful_stop();
            let elapsed = started.elapsed();

            let bystander = recycler.join().expect("recycler thread");
            let staged = bystander.as_ref().is_some_and(|b| b.id() == victim)
                // GRACEFUL_SHUTDOWN_TIMEOUT_MS is 2000; anything materially
                // shorter means the poll saw the pid empty and returned.
                && elapsed >= Duration::from_millis(1500);

            // Read the bystander's own exit status rather than sampling
            // `/proc` the instant `graceful_stop` returns: a SIGKILL sent
            // microseconds earlier has not necessarily landed yet, and that
            // sampling races into reporting a killed bystander as alive.
            let killed_by = bystander.and_then(|mut bystander| {
                let settle = Instant::now() + Duration::from_millis(500);
                loop {
                    match bystander.try_wait() {
                        Ok(Some(status)) => return status.signal(),
                        Ok(None) if Instant::now() >= settle => {
                            let _ = bystander.kill();
                            let _ = bystander.wait();
                            return None;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                        Err(_) => return None,
                    }
                }
            });

            let _ = helper.kill();
            let _ = helper.wait();

            if !staged {
                eprintln!(
                    "attempt {attempt}: pid reuse did not land (elapsed {elapsed:?}); retrying"
                );
                continue;
            }

            assert_eq!(
                killed_by, None,
                "graceful_stop's timeout SIGKILL landed on pid {victim} after it had \
                 been recycled: the entry guard passed for the process that used to \
                 own that number, and nothing re-checked before the kill"
            );
            return;
        }

        panic!("could not stage pid reuse in {ATTEMPTS} attempts");
    }
}
