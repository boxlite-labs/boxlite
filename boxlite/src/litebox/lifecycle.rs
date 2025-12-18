//! Box lifecycle management
//!
//! Handles state transitions, shutdown, cleanup, and Drop implementation.

use super::LiteBox;
use super::init::BoxInner;
use crate::{BoxInfo, BoxState};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::fs;
use std::sync::atomic::Ordering;

/// Ensure the box is fully initialized and ready for operations.
pub(super) async fn ensure_ready(litebox: &LiteBox) -> BoxliteResult<&BoxInner> {
    litebox
        .inner
        .get_or_try_init(|| async {
            let builder = litebox
                .builder
                .lock()
                .await
                .take()
                .ok_or_else(|| BoxliteError::Internal("builder already consumed".into()))?;
            builder.build().await
        })
        .await
}

/// Get current information about this box.
pub(crate) fn info(litebox: &LiteBox) -> BoxliteResult<BoxInfo> {
    // Acquire read lock
    let state = litebox.runtime.acquire_read()?;

    // Call BoxManager method directly and convert to BoxInfo
    state
        .box_manager
        .get(&litebox.id)?
        .map(|m| m.to_info())
        .ok_or_else(|| BoxliteError::Internal("box not found in manager".into()))
}

/// Gracefully shut down the box.
pub(crate) async fn shutdown(litebox: &LiteBox) -> BoxliteResult<bool> {
    // Use atomic compare-exchange to ensure shutdown only runs once
    match litebox
        .is_shutdown
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
    {
        Ok(_) => {
            tracing::debug!("shutdown to the litebox {}", litebox.id);
        }
        Err(_) => {
            return Ok(false);
        }
    }

    // Mark as stopped in manager (acquire write lock)
    {
        let state = litebox.runtime.acquire_write()?;
        let _ = state
            .box_manager
            .update_state(&litebox.id, BoxState::Stopped);
    }

    // Only proceed with shutdown if initialized
    if let Some(inner) = litebox.inner.get() {
        // Gracefully shut down guest first
        if let Ok(mut guest_interface) = inner.guest_session.guest().await {
            let _ = guest_interface.shutdown().await;
        }

        // Stop controller (terminates Box subprocess)
        if let Ok(mut controller) = inner.controller.lock() {
            controller.stop()?;
        }

        // Socket cleanup is handled automatically when process exits

        // Clean up box directory
        if let Err(e) = fs::remove_dir_all(&inner.box_home) {
            tracing::warn!("Failed to cleanup box directory in shutdown: {}", e);
        }
    }

    Ok(true)
}

/// Drop handler - ensures shutdown was called before drop.
pub(crate) fn drop_handler(litebox: &mut LiteBox) {
    tracing::debug!("LiteBox::drop called for box_id={}", litebox.id);

    // Assert that shutdown was called before Drop
    // The caller MUST call shutdown() explicitly before dropping
    assert!(
        litebox.is_shutdown.load(Ordering::SeqCst),
        "LiteBox dropped without calling shutdown() first! box_id={}",
        litebox.id
    );
}
