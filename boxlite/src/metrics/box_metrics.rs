//! Per-box metrics (individual LiteBox statistics).

use std::sync::atomic::{AtomicU64, Ordering};

/// Storage for per-box metrics.
///
/// Stored in `BoxMetadata`, one instance per box.
/// All counters are monotonic (never decrease).
#[derive(Default, Debug)]
pub struct BoxMetricsStorage {
    /// Commands executed on this box
    pub(crate) commands_executed: AtomicU64,
    /// Command execution errors on this box
    pub(crate) exec_errors: AtomicU64,
    /// Bytes sent to this box (via stdin)
    pub(crate) bytes_sent: AtomicU64,
    /// Bytes received from this box (via stdout/stderr)
    pub(crate) bytes_received: AtomicU64,

    // Timing metrics (set once, never change)
    /// Total time from create() call to LiteBox ready (includes all stages)
    pub(crate) total_create_duration_ms: Option<u128>,
    /// Time from box subprocess spawn to guest agent ready
    pub(crate) guest_boot_duration_ms: Option<u128>,

    // Stage-level timing breakdown (set once during initialization)
    /// Time to create box directory structure (Stage 1)
    pub(crate) stage_filesystem_setup_ms: Option<u128>,
    /// Time to pull and prepare container image layers (Stage 2)
    pub(crate) stage_image_prepare_ms: Option<u128>,
    /// Time to bootstrap guest rootfs (Stage 3, lazy initialization)
    pub(crate) stage_guest_rootfs_ms: Option<u128>,
    /// Time to build box configuration (Stage 4)
    pub(crate) stage_box_config_ms: Option<u128>,
    /// Time to spawn box subprocess (Stage 5, excludes guest boot)
    pub(crate) stage_box_spawn_ms: Option<u128>,
    /// Time to initialize container inside guest (Stage 6)
    pub(crate) stage_container_init_ms: Option<u128>,
}

impl Clone for BoxMetricsStorage {
    fn clone(&self) -> Self {
        Self {
            commands_executed: AtomicU64::new(self.commands_executed.load(Ordering::Relaxed)),
            exec_errors: AtomicU64::new(self.exec_errors.load(Ordering::Relaxed)),
            bytes_sent: AtomicU64::new(self.bytes_sent.load(Ordering::Relaxed)),
            bytes_received: AtomicU64::new(self.bytes_received.load(Ordering::Relaxed)),
            total_create_duration_ms: self.total_create_duration_ms,
            guest_boot_duration_ms: self.guest_boot_duration_ms,
            stage_filesystem_setup_ms: self.stage_filesystem_setup_ms,
            stage_image_prepare_ms: self.stage_image_prepare_ms,
            stage_guest_rootfs_ms: self.stage_guest_rootfs_ms,
            stage_box_config_ms: self.stage_box_config_ms,
            stage_box_spawn_ms: self.stage_box_spawn_ms,
            stage_container_init_ms: self.stage_container_init_ms,
        }
    }
}

impl BoxMetricsStorage {
    /// Create new per-box metrics storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set total create duration (called once during box creation).
    pub(crate) fn set_total_create_duration(&mut self, duration_ms: u128) {
        self.total_create_duration_ms = Some(duration_ms);
    }

    /// Set guest boot duration (called once after guest is ready).
    #[allow(dead_code)] // API designed but not yet wired up
    pub(crate) fn set_guest_boot_duration(&mut self, duration_ms: u128) {
        self.guest_boot_duration_ms = Some(duration_ms);
    }

    /// Set filesystem setup stage duration.
    pub(crate) fn set_stage_filesystem_setup(&mut self, duration_ms: u128) {
        self.stage_filesystem_setup_ms = Some(duration_ms);
    }

    /// Set image preparation stage duration.
    pub(crate) fn set_stage_image_prepare(&mut self, duration_ms: u128) {
        self.stage_image_prepare_ms = Some(duration_ms);
    }

    /// Set guest rootfs bootstrap stage duration.
    pub(crate) fn set_stage_guest_rootfs(&mut self, duration_ms: u128) {
        self.stage_guest_rootfs_ms = Some(duration_ms);
    }

    /// Set box config build stage duration.
    #[allow(dead_code)] // API designed but not yet wired up
    pub(crate) fn set_stage_box_config(&mut self, duration_ms: u128) {
        self.stage_box_config_ms = Some(duration_ms);
    }

    /// Set box spawn stage duration.
    pub(crate) fn set_stage_box_spawn(&mut self, duration_ms: u128) {
        self.stage_box_spawn_ms = Some(duration_ms);
    }

    /// Set container initialization stage duration.
    pub(crate) fn set_stage_container_init(&mut self, duration_ms: u128) {
        self.stage_container_init_ms = Some(duration_ms);
    }

    /// Log init stage durations for debugging.
    pub(crate) fn log_init_stages(&self) {
        tracing::debug!(
            total_create_duration_ms = self.total_create_duration_ms.unwrap_or(0),
            stage_filesystem_setup_ms = self.stage_filesystem_setup_ms.unwrap_or(0),
            stage_image_prepare_ms = self.stage_image_prepare_ms.unwrap_or(0),
            stage_guest_rootfs_ms = self.stage_guest_rootfs_ms.unwrap_or(0),
            stage_box_config_ms = self.stage_box_config_ms.unwrap_or(0),
            stage_box_spawn_ms = self.stage_box_spawn_ms.unwrap_or(0),
            stage_container_init_ms = self.stage_container_init_ms.unwrap_or(0),
            "Box initialization stages completed"
        );
    }

    /// Increment commands executed counter.
    pub(crate) fn increment_commands_executed(&self) {
        self.commands_executed.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment execution errors counter.
    pub(crate) fn increment_exec_errors(&self) {
        self.exec_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Add bytes sent to counter.
    #[allow(dead_code)]
    pub(crate) fn add_bytes_sent(&self, bytes: u64) {
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Add bytes received to counter.
    #[allow(dead_code)]
    pub(crate) fn add_bytes_received(&self, bytes: u64) {
        self.bytes_received.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Handle for querying per-box metrics.
///
/// Snapshot of metrics at query time.
/// All counters are monotonic and never reset.
#[derive(Clone, Debug)]
pub struct BoxMetrics {
    /// Commands executed on this box
    pub commands_executed_total: u64,
    /// Command execution errors on this box
    pub exec_errors_total: u64,
    /// Bytes sent to this box (via stdin)
    pub bytes_sent_total: u64,
    /// Bytes received from this box (via stdout/stderr)
    pub bytes_received_total: u64,
    /// Total time from create() call to LiteBox ready (milliseconds)
    pub total_create_duration_ms: Option<u128>,
    /// Time from box subprocess spawn to guest agent ready (milliseconds)
    pub guest_boot_duration_ms: Option<u128>,
    /// CPU usage percent (0.0-100.0)
    pub cpu_percent: Option<f32>,
    /// Memory usage in bytes
    pub memory_bytes: Option<u64>,
    /// Network bytes sent (host to guest)
    pub network_bytes_sent: Option<u64>,
    /// Network bytes received (guest to host)
    pub network_bytes_received: Option<u64>,
    /// Current TCP connections
    pub network_tcp_connections: Option<u64>,
    /// Total TCP connection errors
    pub network_tcp_errors: Option<u64>,

    // Stage-level timing breakdown
    /// Time to create box directory structure (milliseconds)
    pub stage_filesystem_setup_ms: Option<u128>,
    /// Time to pull and prepare container image layers (milliseconds)
    pub stage_image_prepare_ms: Option<u128>,
    /// Time to bootstrap guest rootfs (milliseconds)
    pub stage_guest_rootfs_ms: Option<u128>,
    /// Time to build box configuration (milliseconds)
    pub stage_box_config_ms: Option<u128>,
    /// Time to spawn box subprocess (milliseconds)
    pub stage_box_spawn_ms: Option<u128>,
    /// Time to initialize container inside guest (milliseconds)
    pub stage_container_init_ms: Option<u128>,
}

impl BoxMetrics {
    /// Create snapshot from storage and system metrics.
    pub(crate) fn from_storage(
        storage: &BoxMetricsStorage,
        cpu_percent: Option<f32>,
        memory_bytes: Option<u64>,
        network_bytes_sent: Option<u64>,
        network_bytes_received: Option<u64>,
        network_tcp_connections: Option<u64>,
        network_tcp_errors: Option<u64>,
    ) -> Self {
        Self {
            commands_executed_total: storage.commands_executed.load(Ordering::Relaxed),
            exec_errors_total: storage.exec_errors.load(Ordering::Relaxed),
            bytes_sent_total: storage.bytes_sent.load(Ordering::Relaxed),
            bytes_received_total: storage.bytes_received.load(Ordering::Relaxed),
            total_create_duration_ms: storage.total_create_duration_ms,
            guest_boot_duration_ms: storage.guest_boot_duration_ms,
            cpu_percent,
            memory_bytes,
            network_bytes_sent,
            network_bytes_received,
            network_tcp_connections,
            network_tcp_errors,
            stage_filesystem_setup_ms: storage.stage_filesystem_setup_ms,
            stage_image_prepare_ms: storage.stage_image_prepare_ms,
            stage_guest_rootfs_ms: storage.stage_guest_rootfs_ms,
            stage_box_config_ms: storage.stage_box_config_ms,
            stage_box_spawn_ms: storage.stage_box_spawn_ms,
            stage_container_init_ms: storage.stage_container_init_ms,
        }
    }

    /// Total commands executed on this box.
    ///
    /// Incremented on every `exec()` call.
    /// Never decreases (monotonic counter).
    pub fn commands_executed_total(&self) -> u64 {
        self.commands_executed_total
    }

    /// Total command execution errors on this box.
    ///
    /// Incremented when `exec()` returns error.
    /// Never decreases (monotonic counter).
    pub fn exec_errors_total(&self) -> u64 {
        self.exec_errors_total
    }

    /// Total bytes sent to this box (stdin).
    ///
    /// Never decreases (monotonic counter).
    pub fn bytes_sent_total(&self) -> u64 {
        self.bytes_sent_total
    }

    /// Total bytes received from this box (stdout/stderr).
    ///
    /// Never decreases (monotonic counter).
    pub fn bytes_received_total(&self) -> u64 {
        self.bytes_received_total
    }

    /// Total time from create() call to box ready (milliseconds).
    ///
    /// Includes all initialization stages: filesystem setup, image pull,
    /// guest rootfs bootstrap, box config, box spawn, and container init.
    /// Returns None if box not yet initialized.
    pub fn total_create_duration_ms(&self) -> Option<u128> {
        self.total_create_duration_ms
    }

    /// Time from box subprocess spawn to guest agent ready (milliseconds).
    ///
    /// Measures guest boot time only (excludes image preparation).
    /// Returns None if guest not yet ready.
    pub fn guest_boot_duration_ms(&self) -> Option<u128> {
        self.guest_boot_duration_ms
    }

    /// CPU usage percent (0.0-100.0).
    ///
    /// Returns None if box not started or process not found.
    pub fn cpu_percent(&self) -> Option<f32> {
        self.cpu_percent
    }

    /// Memory usage in bytes.
    ///
    /// Returns None if box not started or process not found.
    pub fn memory_bytes(&self) -> Option<u64> {
        self.memory_bytes
    }

    /// Network bytes sent from host to guest.
    ///
    /// Returns None if network backend doesn't support metrics.
    pub fn network_bytes_sent(&self) -> Option<u64> {
        self.network_bytes_sent
    }

    /// Network bytes received from guest to host.
    ///
    /// Returns None if network backend doesn't support metrics.
    pub fn network_bytes_received(&self) -> Option<u64> {
        self.network_bytes_received
    }

    /// Current TCP connections in ESTABLISHED state.
    ///
    /// Returns None if network backend doesn't support metrics.
    pub fn network_tcp_connections(&self) -> Option<u64> {
        self.network_tcp_connections
    }

    /// Total failed TCP connection attempts.
    ///
    /// Returns None if network backend doesn't support metrics.
    pub fn network_tcp_errors(&self) -> Option<u64> {
        self.network_tcp_errors
    }

    // Stage-level timing getters

    /// Time to create box directory structure (milliseconds).
    ///
    /// Stage 1 of initialization pipeline.
    /// Returns None if stage not yet completed.
    pub fn stage_filesystem_setup_ms(&self) -> Option<u128> {
        self.stage_filesystem_setup_ms
    }

    /// Time to pull and prepare container image layers (milliseconds).
    ///
    /// Stage 2 of initialization pipeline.
    /// Includes image pull (if not cached) and layer extraction.
    /// Returns None if stage not yet completed.
    pub fn stage_image_prepare_ms(&self) -> Option<u128> {
        self.stage_image_prepare_ms
    }

    /// Time to bootstrap guest rootfs (milliseconds).
    ///
    /// Stage 3 of initialization pipeline.
    /// Only non-zero on first box creation (lazy initialization).
    /// Returns None if stage not yet completed.
    pub fn stage_guest_rootfs_ms(&self) -> Option<u128> {
        self.stage_guest_rootfs_ms
    }

    /// Time to build box configuration (milliseconds).
    ///
    /// Stage 4 of initialization pipeline.
    /// Includes disk setup and network configuration.
    /// Returns None if stage not yet completed.
    pub fn stage_box_config_ms(&self) -> Option<u128> {
        self.stage_box_config_ms
    }

    /// Time to spawn box subprocess (milliseconds).
    ///
    /// Stage 5 of initialization pipeline.
    /// Includes subprocess spawn and waiting for guest agent.
    /// Returns None if stage not yet completed.
    pub fn stage_box_spawn_ms(&self) -> Option<u128> {
        self.stage_box_spawn_ms
    }

    /// Time to initialize container inside guest (milliseconds).
    ///
    /// Stage 6 of initialization pipeline.
    /// Includes rootfs mount and container creation.
    /// Returns None if stage not yet completed.
    pub fn stage_container_init_ms(&self) -> Option<u128> {
        self.stage_container_init_ms
    }
}
