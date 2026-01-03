use boxlite::metrics::{BoxMetrics, RuntimeMetrics};
use napi_derive::napi;

/// Runtime-level metrics snapshot.
///
/// Provides monotonic counters tracking activity across all boxes
/// managed by this runtime instance.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsRuntimeMetrics {
    /// Total boxes created since runtime startup
    pub boxes_created_total: f64,
    /// Total boxes that failed during creation/initialization
    pub boxes_failed_total: f64,
    /// Number of currently running boxes
    pub num_running_boxes: f64,
    /// Total commands executed across all boxes
    pub total_commands_executed: f64,
    /// Total command execution errors across all boxes
    pub total_exec_errors: f64,
}

impl From<RuntimeMetrics> for JsRuntimeMetrics {
    fn from(m: RuntimeMetrics) -> Self {
        Self {
            boxes_created_total: m.boxes_created_total() as f64,
            boxes_failed_total: m.boxes_failed_total() as f64,
            num_running_boxes: m.num_running_boxes() as f64,
            total_commands_executed: m.total_commands_executed() as f64,
            total_exec_errors: m.total_exec_errors() as f64,
        }
    }
}

/// Box-level metrics snapshot.
///
/// Provides detailed metrics about a specific box's resource usage,
/// network activity, and lifecycle timing.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsBoxMetrics {
    // Execution metrics
    /// Commands executed on this box
    pub commands_executed_total: f64,
    /// Command execution errors on this box
    pub exec_errors_total: f64,
    /// Bytes sent to this box (via stdin)
    pub bytes_sent_total: f64,
    /// Bytes received from this box (via stdout/stderr)
    pub bytes_received_total: f64,

    // Lifecycle timing
    /// Total time from create() call to LiteBox ready (milliseconds)
    pub total_create_duration_ms: Option<f64>,
    /// Time from box subprocess spawn to guest agent ready (milliseconds)
    pub guest_boot_duration_ms: Option<f64>,

    // Resource usage (runtime, may be None if not available)
    /// CPU usage percent (0.0-100.0)
    pub cpu_percent: Option<f64>,
    /// Memory usage in bytes
    pub memory_bytes: Option<f64>,

    // Network metrics
    /// Network bytes sent (host to guest)
    pub network_bytes_sent: Option<f64>,
    /// Network bytes received (guest to host)
    pub network_bytes_received: Option<f64>,
    /// Current TCP connections
    pub network_tcp_connections: Option<f64>,
    /// Total TCP connection errors
    pub network_tcp_errors: Option<f64>,

    // Stage-level timing breakdown
    /// Time to create box directory structure (milliseconds)
    pub stage_filesystem_setup_ms: Option<f64>,
    /// Time to pull and prepare container image layers (milliseconds)
    pub stage_image_prepare_ms: Option<f64>,
    /// Time to bootstrap guest rootfs (milliseconds)
    pub stage_guest_rootfs_ms: Option<f64>,
    /// Time to build box configuration (milliseconds)
    pub stage_box_config_ms: Option<f64>,
    /// Time to spawn box subprocess (milliseconds)
    pub stage_box_spawn_ms: Option<f64>,
    /// Time to initialize container inside guest (milliseconds)
    pub stage_container_init_ms: Option<f64>,
}

impl From<BoxMetrics> for JsBoxMetrics {
    fn from(m: BoxMetrics) -> Self {
        Self {
            // Execution metrics
            commands_executed_total: m.commands_executed_total as f64,
            exec_errors_total: m.exec_errors_total as f64,
            bytes_sent_total: m.bytes_sent_total as f64,
            bytes_received_total: m.bytes_received_total as f64,

            // Lifecycle timing (convert u128 to f64 for JavaScript)
            total_create_duration_ms: m.total_create_duration_ms.map(|v| v as f64),
            guest_boot_duration_ms: m.guest_boot_duration_ms.map(|v| v as f64),

            // Resource usage
            cpu_percent: m.cpu_percent.map(|v| v as f64),
            memory_bytes: m.memory_bytes.map(|v| v as f64),

            // Network metrics (convert u64 to f64 for JavaScript)
            network_bytes_sent: m.network_bytes_sent.map(|v| v as f64),
            network_bytes_received: m.network_bytes_received.map(|v| v as f64),
            network_tcp_connections: m.network_tcp_connections.map(|v| v as f64),
            network_tcp_errors: m.network_tcp_errors.map(|v| v as f64),

            // Stage timing (convert u128 to f64 for JavaScript)
            stage_filesystem_setup_ms: m.stage_filesystem_setup_ms.map(|v| v as f64),
            stage_image_prepare_ms: m.stage_image_prepare_ms.map(|v| v as f64),
            stage_guest_rootfs_ms: m.stage_guest_rootfs_ms.map(|v| v as f64),
            stage_box_config_ms: m.stage_box_config_ms.map(|v| v as f64),
            stage_box_spawn_ms: m.stage_box_spawn_ms.map(|v| v as f64),
            stage_container_init_ms: m.stage_container_init_ms.map(|v| v as f64),
        }
    }
}
