//! Execution service interface.
//!
//! High-level API for execution operations (unary Exec + output-only Attach +
//! blocking Wait).

use crate::litebox::{BoxCommand, ExecResult};
use boxlite_shared::{
    AttachRequest, BoxliteError, BoxliteResult, ExecOutput, ExecRequest, ExecStdin,
    ExecutionClient, KillRequest, WaitRequest, WaitResponse, exec_output,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

/// Execution service interface.
#[derive(Clone)]
pub struct ExecutionInterface {
    client: ExecutionClient<Channel>,
}

/// Components for building an Execution.
pub struct ExecComponents {
    pub execution_id: String,
    pub stdin_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub stdout_rx: mpsc::UnboundedReceiver<String>,
    pub stderr_rx: mpsc::UnboundedReceiver<String>,
    pub result_rx: mpsc::UnboundedReceiver<ExecResult>,
}

impl ExecutionInterface {
    /// Create from a channel.
    pub fn new(channel: Channel) -> Self {
        Self {
            client: ExecutionClient::new(channel),
        }
    }

    /// Execute a command and return execution components.
    pub async fn exec(&mut self, command: BoxCommand) -> BoxliteResult<ExecComponents> {
        // Create channels
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel::<String>();
        let (result_tx, result_rx) = mpsc::unbounded_channel();

        // Build request
        let request = ExecProtocol::build_exec_request(&command);

        tracing::debug!(?command, "Starting execution");

        // Start execution
        let exec_response = self.client.exec(request).await?.into_inner();
        if let Some(err) = exec_response.error {
            return Err(BoxliteError::Internal(format!(
                "{}: {}",
                err.reason, err.detail
            )));
        }

        let execution_id = exec_response.execution_id.clone();

        // Spawn stdin pump
        ExecProtocol::spawn_stdin(self.client.clone(), execution_id.clone(), stdin_rx);

        // Spawn attach fanout
        ExecProtocol::spawn_attach(
            self.client.clone(),
            execution_id.clone(),
            stdout_tx,
            stderr_tx,
        );

        // Spawn wait task for terminal status
        ExecProtocol::spawn_wait(self.client.clone(), execution_id.clone(), result_tx);

        Ok(ExecComponents {
            execution_id,
            stdin_tx,
            stdout_rx,
            stderr_rx,
            result_rx,
        })
    }

    /// Wait for execution to complete.
    #[allow(dead_code)] // API method for future use
    pub async fn wait(&mut self, execution_id: &str) -> BoxliteResult<ExecResult> {
        let request = WaitRequest {
            execution_id: execution_id.to_string(),
        };

        let response = self.client.wait(request).await?.into_inner();
        Ok(ExecProtocol::map_wait_response(response))
    }

    /// Kill execution (send signal).
    pub async fn kill(&mut self, execution_id: &str, signal: i32) -> BoxliteResult<()> {
        let request = KillRequest {
            execution_id: execution_id.to_string(),
            signal,
        };

        let response = self.client.kill(request).await?.into_inner();

        if response.success {
            Ok(())
        } else {
            Err(BoxliteError::Internal(
                response.error.unwrap_or_else(|| "Kill failed".to_string()),
            ))
        }
    }

    /// Resize PTY terminal window.
    pub async fn resize_tty(
        &mut self,
        execution_id: &str,
        rows: u32,
        cols: u32,
        x_pixels: u32,
        y_pixels: u32,
    ) -> BoxliteResult<()> {
        use boxlite_shared::ResizeTtyRequest;

        let request = ResizeTtyRequest {
            execution_id: execution_id.to_string(),
            rows,
            cols,
            x_pixels,
            y_pixels,
        };

        let response = self.client.resize_tty(request).await?.into_inner();

        if response.success {
            Ok(())
        } else {
            Err(BoxliteError::Internal(
                response
                    .error
                    .unwrap_or_else(|| "Resize TTY failed".to_string()),
            ))
        }
    }
}

// ============================================================================
// Helper: Protocol wiring
// ============================================================================

struct ExecProtocol;

impl ExecProtocol {
    fn build_exec_request(command: &BoxCommand) -> ExecRequest {
        use boxlite_shared::TtyConfig;

        ExecRequest {
            execution_id: None,
            program: command.command.clone(),
            args: command.args.clone(),
            env: command
                .env
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            workdir: command.working_dir.clone().unwrap_or_default(),
            timeout_ms: command.timeout.map(|d| d.as_millis() as u64).unwrap_or(0),
            tty: if command.tty {
                let (rows, cols) = crate::util::get_terminal_size();
                Some(TtyConfig {
                    rows,
                    cols,
                    x_pixels: 0,
                    y_pixels: 0,
                })
            } else {
                None
            },
        }
    }

    fn map_wait_response(resp: WaitResponse) -> ExecResult {
        let code = if resp.signal != 0 {
            -resp.signal
        } else {
            resp.exit_code
        };
        ExecResult { exit_code: code }
    }

    fn spawn_attach(
        mut client: ExecutionClient<Channel>,
        execution_id: String,
        stdout_tx: mpsc::UnboundedSender<String>,
        stderr_tx: mpsc::UnboundedSender<String>,
    ) {
        tokio::spawn(async move {
            let request = AttachRequest {
                execution_id: execution_id.clone(),
            };

            match client.attach(request).await {
                Ok(response) => {
                    tracing::debug!(execution_id = %execution_id, "Attach stream connected");
                    let mut stream = response.into_inner();
                    let mut message_count = 0u64;
                    while let Some(output) = stream.message().await.transpose() {
                        match output {
                            Ok(output) => {
                                message_count += 1;
                                Self::route_output(output, &stdout_tx, &stderr_tx);
                            }
                            Err(e) => {
                                tracing::debug!(
                                    execution_id = %execution_id,
                                    error = %e,
                                    message_count,
                                    "Attach stream error, breaking"
                                );
                                let _ = stderr_tx.send(format!("Attach stream error: {}", e));
                                break;
                            }
                        }
                    }
                    tracing::debug!(
                        execution_id = %execution_id,
                        message_count,
                        "Attach stream ended normally"
                    );
                }
                Err(e) => {
                    tracing::debug!(execution_id = %execution_id, error = %e, "Attach failed");
                    let _ = stderr_tx.send(format!("Attach failed: {}", e));
                }
            }
        });
    }

    fn route_output(
        output: ExecOutput,
        stdout_tx: &mpsc::UnboundedSender<String>,
        stderr_tx: &mpsc::UnboundedSender<String>,
    ) {
        match output.event {
            Some(exec_output::Event::Stdout(chunk)) => {
                let stdout_data = String::from_utf8_lossy(&chunk.data).to_string();
                tracing::trace!(?stdout_data, "Received exec stdout");
                let _ = stdout_tx.send(stdout_data);
            }
            Some(exec_output::Event::Stderr(chunk)) => {
                let stderr_data = String::from_utf8_lossy(&chunk.data).to_string();
                tracing::trace!(?stderr_data, "Received exec stderr");
                let _ = stderr_tx.send(stderr_data);
            }
            None => {}
        }
    }

    fn spawn_wait(
        mut client: ExecutionClient<Channel>,
        execution_id: String,
        result_tx: mpsc::UnboundedSender<ExecResult>,
    ) {
        tokio::spawn(async move {
            let request = WaitRequest {
                execution_id: execution_id.clone(),
            };

            match client.wait(request).await {
                Ok(resp) => {
                    let mapped = Self::map_wait_response(resp.into_inner());
                    let _ = result_tx.send(mapped);
                }
                Err(e) => {
                    tracing::error!(
                        execution_id = %execution_id,
                        error = %e,
                        "Wait failed"
                    );
                    let _ = result_tx.send(ExecResult { exit_code: -1 });
                }
            }
        });
    }

    fn spawn_stdin(
        mut client: ExecutionClient<Channel>,
        execution_id: String,
        mut stdin_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    ) {
        tokio::spawn(async move {
            let (tx, rx) = mpsc::channel::<ExecStdin>(8);

            // Producer: forward stdin channel into tonic stream
            let exec_id_clone = execution_id.clone();
            tokio::spawn(async move {
                while let Some(data) = stdin_rx.recv().await {
                    let msg = ExecStdin {
                        execution_id: exec_id_clone.clone(),
                        data,
                        close: false,
                    };
                    if tx.send(msg).await.is_err() {
                        return;
                    }
                }

                // Signal stdin close
                let _ = tx
                    .send(ExecStdin {
                        execution_id: exec_id_clone,
                        data: Vec::new(),
                        close: true,
                    })
                    .await;
            });

            let stream = ReceiverStream::new(rx);
            if let Err(e) = client.send_input(stream).await {
                tracing::warn!(
                    execution_id = %execution_id,
                    error = %e,
                    "SendInput failed"
                );
            }
        });
    }
}
