//! Box lifecycle management
//!
//! Handles state transitions, shutdown, cleanup, and Drop implementation.

use super::LiteBox;
use crate::BoxInfo;
use crate::litebox::inner::BoxInner;
use crate::runtime::types::BoxStatus;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Ensure the box is fully initialized and ready for operations.
///
/// Uses double-checked locking pattern for efficient lazy initialization.
/// Returns an Arc clone to allow concurrent access without holding the lock.
///
/// Handles three cases based on box status:
/// - Starting/Stopped: Build using builder (Stopped reuses existing rootfs)
/// - Running: Reattach to already-running VM
pub(super) async fn ensure_ready(litebox: &LiteBox) -> BoxliteResult<Arc<BoxInner>> {
    tracing::trace!(box_id = %litebox.id, "ensure_ready called");

    // Fast path: already initialized (read lock)
    {
        let inner_guard = litebox.inner.read().await;
        if let Some(inner) = inner_guard.as_ref() {
            tracing::trace!(box_id = %litebox.id, "Already initialized, returning existing inner");
            return Ok(Arc::clone(inner));
        }
    }

    tracing::trace!(box_id = %litebox.id, "Not initialized, getting state from manager");

    // Get current state from manager to determine action
    let (config, state) = litebox
        .runtime
        .box_manager
        .get(&litebox.id)?
        .ok_or_else(|| BoxliteError::NotFound(litebox.id.to_string()))?;

    tracing::debug!(
        box_id = %litebox.id,
        status = ?state.status,
        pid = ?state.pid,
        "Box state retrieved, determining initialization path"
    );

    match state.status {
        BoxStatus::Starting | BoxStatus::Stopped => {
            // Starting: new box, normal init
            // Stopped: restart, builder has reuse_rootfs=true
            tracing::debug!(box_id = %litebox.id, status = ?state.status, "Initializing from builder");
            init_from_builder(litebox).await
        }
        BoxStatus::Running => {
            // VM is running but we don't have a handle - reattach
            tracing::debug!(box_id = %litebox.id, pid = ?state.pid, "Reattaching to running box");
            reattach_to_running(litebox, &config, &state).await
        }
        other => Err(BoxliteError::InvalidState(format!(
            "Cannot initialize box in {:?} state",
            other
        ))),
    }
}

/// Initialize from builder (Starting or Stopped state).
async fn init_from_builder(litebox: &LiteBox) -> BoxliteResult<Arc<BoxInner>> {
    let mut inner_guard = litebox.inner.write().await;

    // Double-check: another thread may have initialized while we waited
    if let Some(inner) = inner_guard.as_ref() {
        return Ok(Arc::clone(inner));
    }

    // Consume builder
    let builder = litebox
        .builder
        .lock()
        .await
        .take()
        .ok_or_else(|| BoxliteError::Internal("builder already consumed".into()))?;

    let box_inner = builder.build().await?;
    let arc_inner = Arc::new(box_inner);
    *inner_guard = Some(Arc::clone(&arc_inner));

    Ok(arc_inner)
}

/// Reattach to a running VM (Running state from get()).
async fn reattach_to_running(
    litebox: &LiteBox,
    config: &crate::litebox::config::BoxConfig,
    state: &crate::runtime::types::BoxState,
) -> BoxliteResult<Arc<BoxInner>> {
    tracing::trace!(box_id = %litebox.id, "reattach_to_running: acquiring write lock");
    let mut inner_guard = litebox.inner.write().await;

    // Double-check: another thread may have initialized while we waited
    if let Some(inner) = inner_guard.as_ref() {
        tracing::trace!(box_id = %litebox.id, "reattach_to_running: already initialized by another thread");
        return Ok(Arc::clone(inner));
    }

    let pid = state
        .pid
        .ok_or_else(|| BoxliteError::InvalidState("Running box has no PID".into()))?;

    tracing::info!(box_id = %litebox.id, pid = pid, "Reattaching to running box (VmmAttachTask path)");

    // Verify process is actually alive
    if !crate::util::is_process_alive(pid) {
        // Process died - mark as crashed and return error
        tracing::warn!(box_id = %litebox.id, pid = pid, "Process is dead, marking as crashed");
        let _ = litebox.runtime.box_manager.mark_crashed(&litebox.id);
        return Err(BoxliteError::InvalidState(
            "Box process is no longer running".into(),
        ));
    }

    tracing::debug!(box_id = %litebox.id, pid = pid, "Process is alive, calling BoxInner::reconnect");

    // Reconnect to existing VM
    let box_inner = BoxInner::reconnect(config, state, pid).await?;
    let arc_inner = Arc::new(box_inner);
    *inner_guard = Some(Arc::clone(&arc_inner));

    tracing::info!(box_id = %litebox.id, "Successfully reattached to running box");
    Ok(arc_inner)
}

/// Get current information about this box.
pub(crate) fn info(litebox: &LiteBox) -> BoxliteResult<BoxInfo> {
    litebox
        .runtime
        .box_manager
        .get_info(&litebox.id)?
        .ok_or_else(|| BoxliteError::Internal("box not found in manager".into()))
}

/// Stop the box gracefully without destroying it.
///
/// The VM is stopped but the box directory is preserved. Status is updated to Stopped
/// and can be restarted later.
pub(crate) async fn stop(litebox: &LiteBox) -> BoxliteResult<()> {
    // Mark as shutdown immediately (even if stop fails, prevent Drop panic)
    litebox.is_shutdown.store(true, Ordering::SeqCst);

    // Get initialized state
    let inner_opt = {
        let guard = litebox.inner.read().await;
        guard.as_ref().map(Arc::clone)
    };

    // Verify box is in a stoppable state
    {
        let (_, state) = litebox
            .runtime
            .box_manager
            .get(&litebox.id)?
            .ok_or_else(|| BoxliteError::NotFound(litebox.id.to_string()))?;

        if !state.status.can_stop() {
            return Err(BoxliteError::InvalidState(format!(
                "Cannot stop - box is in {:?} state",
                state.status
            )));
        }
    }

    // If initialized, shut down guest and stop controller
    if let Some(inner) = inner_opt {
        // Gracefully shut down guest
        if let Ok(mut guest_interface) = inner.guest_session.guest().await {
            let _ = guest_interface.shutdown().await;
        }

        // Stop handler (terminates Box subprocess)
        if let Ok(mut handler) = inner.handler.lock() {
            handler.stop()?;
        }
    }
    // If not initialized, nothing to stop - just update state

    // BoxManager encapsulates database-first pattern
    // Coordination lock for multi-step update
    {
        let _guard = litebox.runtime.acquire_write()?;
        litebox
            .runtime
            .box_manager
            .update_status(&litebox.id, BoxStatus::Stopped)?;
        litebox.runtime.box_manager.update_pid(&litebox.id, None)?;
    }

    // Clear the inner state (VM is stopped)
    {
        let mut guard = litebox.inner.write().await;
        *guard = None;
    }

    tracing::info!("Stopped box {}", litebox.id);

    // Auto-remove if configured (like Docker's --rm flag)
    if litebox.auto_remove {
        tracing::debug!(box_id = %litebox.id, "auto_remove enabled, removing box");
        // Box is already stopped, so force=false is sufficient
        litebox.runtime.remove_box(&litebox.id, false)?;
    }

    Ok(())
}

/// Drop handler - ensures proper cleanup was called before drop.
///
/// Panics if the handle is dropped while:
/// - The box is still active (Starting/Running), AND
/// - stop() was not called on THIS handle
///
/// Does NOT panic if:
/// - stop() was called on this handle
/// - The box was removed via runtime.remove(force=true)
/// - The box is already in Stopped state
pub(crate) fn drop_handler(litebox: &mut LiteBox) {
    tracing::debug!("LiteBox::drop called for box_id={}", litebox.id);

    // If stop() was called on this handle, we're good
    if litebox.is_shutdown.load(Ordering::SeqCst) {
        return;
    }

    // Check if box still exists and is active
    // If it was removed via runtime.remove() or is already stopped, allow drop
    match litebox.runtime.box_manager.get(&litebox.id) {
        Ok(Some((_, state))) if state.status.is_active() => {
            // Box is still active and stop() wasn't called - this is a bug
            panic!(
                "LiteBox dropped without cleanup! Call stop() first. box_id={}",
                litebox.id
            );
        }
        _ => {
            // Box was removed, doesn't exist, or is already stopped - allow drop
            tracing::debug!(
                "LiteBox::drop allowing drop without explicit stop() for box_id={} (already cleaned up)",
                litebox.id
            );
        }
    }
}
