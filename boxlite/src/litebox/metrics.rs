//! Box metrics collection and aggregation

use super::LiteBox;
use super::lifecycle;
use crate::litebox::inner::BoxInner;
use crate::metrics::BoxMetrics;
use boxlite_shared::errors::BoxliteResult;
use std::sync::atomic::Ordering;

/// Get unified metrics (operational + system).
///
/// Returns a snapshot of:
/// - Operational metrics: Commands executed, errors, bytes transferred (monotonic counters)
/// - System metrics: CPU usage, memory usage (current values)
/// - Timing metrics: Spawn and boot duration
///
/// Note: Network metrics are not available from host process since gvproxy
/// now runs in the shim subprocess to survive detach operations.
///
/// All operational counters never reset - delta calculation is caller's responsibility.
/// System metrics are fetched fresh on every call.
pub(crate) async fn metrics(litebox: &LiteBox) -> BoxliteResult<BoxMetrics> {
    let inner = lifecycle::ensure_ready(litebox).await?;

    // Fetch system metrics from handler
    let (cpu_percent, memory_bytes) = fetch_system_metrics(&inner.handler)?;

    // Note: Network metrics are not available - gvproxy runs in shim subprocess
    // to survive detach operations. Network stats could be exposed via gRPC in the future.

    // Combine operational (from storage) + system (from controller)
    Ok(BoxMetrics::from_storage(
        &inner.metrics,
        cpu_percent,
        memory_bytes,
        None, // network_bytes_sent - not available from host
        None, // network_bytes_received - not available from host
        None, // network_tcp_connections - not available from host
        None, // network_tcp_errors - not available from host
    ))
}

/// Instrument execution metrics at both box and runtime levels.
pub(super) fn instrument_exec_metrics(litebox: &LiteBox, inner: &BoxInner, is_error: bool) {
    // Level 1: Per-box counter (stored internally in LiteBox, like Tokio's TaskMetrics)
    inner.metrics.increment_commands_executed();
    if is_error {
        inner.metrics.increment_exec_errors();
    }

    // Level 2: Runtime aggregate (lock-free!)
    litebox
        .runtime
        .runtime_metrics
        .total_commands
        .fetch_add(1, Ordering::Relaxed);

    if is_error {
        litebox
            .runtime
            .runtime_metrics
            .total_exec_errors
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Fetch system metrics from handler.
fn fetch_system_metrics(
    handler: &std::sync::Mutex<Box<dyn crate::vmm::controller::VmmHandler>>,
) -> BoxliteResult<(Option<f32>, Option<u64>)> {
    let handler = handler.lock().map_err(|e| {
        boxlite_shared::errors::BoxliteError::Internal(format!("handler lock poisoned: {}", e))
    })?;
    let raw = handler.metrics()?;
    Ok((raw.cpu_percent, raw.memory_bytes))
}
