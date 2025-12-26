#![cfg(target_os = "linux")]
//! Execution service implementation.
//!
//! Provides gRPC service for executing commands in containers with
//! streaming I/O, process control, and state management.
//!
//! ## Architecture
//!
//! This module follows a clean layered design:
//!
//! - **Protocol Layer** (mod.rs): gRPC service implementation
//! - **Executor Layer** (executor.rs): Process spawning abstraction
//! - **Lifecycle Layer** (timeout.rs): Process management
//! - **State Layer** (registry.rs, state.rs): Execution state
//! - **Types** (types.rs): Shared types
//!
//! Each file has a single, clear responsibility.

#[cfg(target_os = "linux")]
pub mod exec_handle;
pub(in crate::service) mod executor;
pub(in crate::service) mod registry;
mod state;
mod timeout;

use crate::service::exec::executor::{ContainerExecutor, GuestExecutor};
use crate::service::server::GuestServer;
use boxlite_shared::{
    constants::executor as executor_const, AttachRequest, ExecError, ExecOutput, ExecRequest,
    ExecResponse, ExecStdin, Execution, KillRequest, KillResponse, ResizeTtyRequest,
    ResizeTtyResponse, SendInputAck, WaitRequest, WaitResponse,
};
use futures::stream::Stream;
use std::pin::Pin;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info};

#[tonic::async_trait]
impl Execution for GuestServer {
    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        let req = request.into_inner();
        let execution_id = req
            .execution_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        info!(
            execution_id = %execution_id,
            program = %req.program,
            "exec request"
        );

        // Validate: execution doesn't already exist
        if self.registry.exists(&execution_id).await {
            return Ok(Response::new(error_response(
                execution_id,
                "execution_exists",
                "Execution already exists",
            )));
        }

        // Spawn execution
        match spawn_execution(self, execution_id, req).await {
            Ok(resp) => Ok(Response::new(resp)),
            Err(err_resp) => Ok(Response::new(err_resp)),
        }
    }

    type AttachStream = Pin<Box<dyn Stream<Item = Result<ExecOutput, Status>> + Send + 'static>>;

    async fn attach(
        &self,
        request: Request<AttachRequest>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let exec_id = request.into_inner().execution_id;
        info!(execution_id = %exec_id, "attach request");

        // Get state from registry
        let state = self
            .registry
            .get(&exec_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Execution not found: {}", exec_id)))?;

        // Call state directly
        let rx = state.attach(&exec_id).await?;

        Ok(Response::new(
            Box::pin(ReceiverStream::new(rx)) as Self::AttachStream
        ))
    }

    async fn send_input(
        &self,
        request: Request<Streaming<ExecStdin>>,
    ) -> Result<Response<SendInputAck>, Status> {
        let mut stream = request.into_inner();

        // First message must carry execution_id
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("Empty stdin stream"))?;

        let exec_id = first.execution_id.clone();
        if exec_id.is_empty() {
            return Err(Status::invalid_argument("execution_id is required"));
        }

        // Get state from registry
        let state = self
            .registry
            .get(&exec_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Execution not found: {}", exec_id)))?;

        // Call state directly
        let task = state.send_input(first, stream).await?;

        // Wait for task to complete
        match task.await {
            Ok(Ok(())) => Ok(Response::new(SendInputAck {})),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(Status::internal(format!("Stdin task panicked: {}", e))),
        }
    }

    async fn wait(&self, request: Request<WaitRequest>) -> Result<Response<WaitResponse>, Status> {
        use exec_handle::ExitStatus;

        let exec_id = request.into_inner().execution_id;
        debug!(execution_id = %exec_id, "wait request");

        // Get state from registry
        let state = self
            .registry
            .get(&exec_id)
            .await
            .ok_or_else(|| Status::not_found(format!("Execution not found: {}", exec_id)))?;

        // Wait for process to exit
        let exit_status = state.wait_process().await?;

        let (exit_code, signal) = match exit_status {
            ExitStatus::Code(code) => {
                debug!(
                    execution_id = %exec_id,
                    exit_code = code,
                    "Process exited with code"
                );
                (code, 0)
            }
            ExitStatus::Signal(sig) => {
                debug!(
                    execution_id = %exec_id,
                    signal = sig as i32,
                    "Process exited due to signal"
                );
                (0, sig as i32)
            }
        };

        Ok(Response::new(WaitResponse {
            exit_code,
            signal,
            timed_out: false,
            duration_ms: 0,
        }))
    }

    async fn kill(&self, request: Request<KillRequest>) -> Result<Response<KillResponse>, Status> {
        use nix::sys::signal::Signal;

        let req = request.into_inner();
        info!(
            execution_id = %req.execution_id,
            signal = req.signal,
            "kill request"
        );

        // Get state from registry
        let state = self.registry.get(&req.execution_id).await.ok_or_else(|| {
            Status::not_found(format!("Execution not found: {}", req.execution_id))
        })?;

        // Parse signal
        let signal = Signal::try_from(req.signal).map_err(|_| {
            Status::invalid_argument(format!("Invalid signal number: {}", req.signal))
        })?;

        // Send signal
        match state.kill(signal).await {
            true => {
                info!(
                    execution_id = %req.execution_id,
                    signal = req.signal,
                    "signal sent"
                );
                Ok(Response::new(KillResponse {
                    success: true,
                    error: None,
                }))
            }
            false => {
                info!(
                    execution_id = %req.execution_id,
                    "failed to send signal"
                );
                Ok(Response::new(KillResponse {
                    success: false,
                    error: Some("Failed to send signal".to_string()),
                }))
            }
        }
    }

    async fn resize_tty(
        &self,
        request: Request<ResizeTtyRequest>,
    ) -> Result<Response<ResizeTtyResponse>, Status> {
        let req = request.into_inner();

        info!(
            execution_id = %req.execution_id,
            rows = req.rows,
            cols = req.cols,
            "resize_tty request"
        );

        // Get state from registry
        let state = self.registry.get(&req.execution_id).await.ok_or_else(|| {
            Status::not_found(format!("Execution not found: {}", req.execution_id))
        })?;

        // Call state directly
        match state
            .resize_pty(
                req.rows as u16,
                req.cols as u16,
                req.x_pixels as u16,
                req.y_pixels as u16,
            )
            .await
        {
            Ok(()) => {
                info!(
                    execution_id = %req.execution_id,
                    rows = req.rows,
                    cols = req.cols,
                    "tty resized"
                );
                Ok(Response::new(ResizeTtyResponse {
                    success: true,
                    error: None,
                }))
            }
            Err(e) => {
                info!(
                    execution_id = %req.execution_id,
                    error = %e,
                    "failed to resize tty"
                );
                Ok(Response::new(ResizeTtyResponse {
                    success: false,
                    error: Some(e.to_string()),
                }))
            }
        }
    }
}

/// Spawn execution (orchestrates full lifecycle).
async fn spawn_execution(
    server: &GuestServer,
    execution_id: String,
    req: ExecRequest,
) -> Result<ExecResponse, ExecResponse> {
    let started_at_ms = now_ms();

    // Step 1: Spawn process using executor selected by BOXLITE_EXECUTOR env var
    let child = spawn_with_executor(server, &req, &execution_id).await?;

    let pid = child.pid().as_raw() as u32;

    // Step 2: Create execution state and register
    let state = state::ExecutionState::new(child);
    server
        .registry
        .register(execution_id.clone(), state.clone())
        .await;

    // Step 3: Start timeout watcher (if requested)
    if req.timeout_ms > 0 {
        timeout::start_timeout_watcher(
            state,
            execution_id.clone(),
            std::time::Duration::from_millis(req.timeout_ms),
        );
    }

    Ok(ExecResponse {
        execution_id,
        pid,
        started_at_ms,
        error: None,
    })
}

fn error_response(id: String, reason: &str, detail: &str) -> ExecResponse {
    ExecResponse {
        execution_id: id,
        pid: 0,
        started_at_ms: 0,
        error: Some(ExecError {
            reason: reason.to_string(),
            detail: detail.to_string(),
        }),
    }
}

fn spawn_error(exec_id: &str, err: String) -> ExecResponse {
    ExecResponse {
        execution_id: exec_id.to_string(),
        pid: 0,
        started_at_ms: 0,
        error: Some(ExecError {
            reason: "spawn_failed".to_string(),
            detail: err,
        }),
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Spawn process with executor selected by BOXLITE_EXECUTOR env var.
///
/// Syntax:
/// - No env var or empty: use guest executor
/// - "guest": run directly on guest VM
/// - "container=<id>": run in container with specified ID
async fn spawn_with_executor(
    server: &GuestServer,
    req: &ExecRequest,
    execution_id: &str,
) -> Result<exec_handle::ExecHandle, ExecResponse> {
    use executor::Executor;

    let executor_value = req.env.get(executor_const::ENV_VAR).map(|s| s.as_str());

    match executor_value {
        Some(executor_const::GUEST) | None | Some("") => {
            // Guest executor (explicit or default)
            debug!(execution_id = %execution_id, "Using GuestExecutor");
            GuestExecutor
                .spawn(req)
                .await
                .map_err(|e| spawn_error(execution_id, e.to_string()))
        }
        Some(s) if s.starts_with(executor_const::CONTAINER_KEY) => {
            // Container executor: parse "container=<id>"
            let container_id = s
                .strip_prefix(executor_const::CONTAINER_KEY)
                .and_then(|rest| rest.strip_prefix('='))
                .unwrap_or("");
            if container_id.is_empty() {
                return Err(spawn_error(
                    execution_id,
                    format!("Invalid {}: missing container_id", executor_const::ENV_VAR),
                ));
            }
            debug!(
                execution_id = %execution_id,
                container_id = %container_id,
                "Using ContainerExecutor"
            );
            // Look up container from registry
            let container_arc = {
                let containers_guard = server.containers.lock().await;
                containers_guard.get(container_id).cloned().ok_or_else(|| {
                    spawn_error(
                        execution_id,
                        format!("Container not found: {}", container_id),
                    )
                })?
            };
            let executor = ContainerExecutor::new(container_arc);
            executor
                .spawn(req)
                .await
                .map_err(|e| spawn_error(execution_id, e.to_string()))
        }
        Some(unknown) => {
            // Unknown executor value
            Err(spawn_error(
                execution_id,
                format!(
                    "Unknown {} value: '{}'. Expected 'guest' or 'container=<id>'",
                    executor_const::ENV_VAR,
                    unknown
                ),
            ))
        }
    }
}
