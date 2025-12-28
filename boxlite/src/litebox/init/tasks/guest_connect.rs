//! Task: Guest Connect - Connect to the guest gRPC session.
//!
//! Creates a GuestSession for communicating with the guest init process.
//! This task is reusable across spawn, restart, and reconnect paths.
//!
//! IMPORTANT: Must wait for guest to be ready before creating session.

use super::{InitCtx, log_task_error, task_start};
use crate::pipeline::PipelineTask;
use crate::portal::GuestSession;
use async_trait::async_trait;
use boxlite_shared::Transport;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::time::Duration;

pub struct GuestConnectTask;

#[async_trait]
impl PipelineTask<InitCtx> for GuestConnectTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (transport, ready_transport, skip_guest_wait) = {
            let ctx = ctx.lock().await;
            (
                ctx.config.transport.clone(),
                Transport::unix(ctx.config.ready_socket_path.clone()),
                ctx.skip_guest_wait,
            )
        };

        // Wait for guest to be ready before creating session
        // Skip for reattach (Running status) - guest already signaled ready at boot
        if skip_guest_wait {
            tracing::debug!(box_id = %box_id, "Skipping guest ready wait (reattach)");
        } else {
            tracing::debug!(box_id = %box_id, "Waiting for guest to be ready");
            wait_for_guest_ready(&ready_transport)
                .await
                .inspect_err(|e| log_task_error(&box_id, task_name, e))?;
        }

        tracing::debug!(box_id = %box_id, "Guest is ready, creating session");
        let guest_session = GuestSession::new(transport);

        let mut ctx = ctx.lock().await;
        ctx.guest_session = Some(guest_session);

        Ok(())
    }

    fn name(&self) -> &str {
        "guest_connect"
    }
}

/// Wait for guest to signal readiness via ready socket.
///
/// Creates a listener on the ready socket and waits for the guest to connect.
/// The guest connects when its gRPC server is ready to serve requests.
async fn wait_for_guest_ready(ready_transport: &boxlite_shared::Transport) -> BoxliteResult<()> {
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
        BoxliteError::Engine(format!(
            "Failed to bind ready socket {}: {}",
            ready_socket_path.display(),
            e
        ))
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
            tracing::debug!("Guest signaled ready via socket connection");
            Ok(())
        }
        Ok(Err(e)) => Err(BoxliteError::Engine(format!(
            "Ready socket accept failed: {}",
            e
        ))),
        Err(_) => Err(BoxliteError::Engine(format!(
            "Timeout waiting for guest ready ({}s)",
            timeout.as_secs()
        ))),
    }
}
