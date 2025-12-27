//! Command execution types
//!
//! Type definitions for executing commands in a box.
//! The actual execution logic is in BoxImpl::exec().

use crate::portal::interfaces::ExecutionInterface;
use boxlite_shared::errors::BoxliteResult;
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;

/// Command builder for executing programs in a box.
///
/// Provides a builder API similar to `std::process::Command`.
///
/// # Examples
///
/// ```rust,no_run
/// # use boxlite::BoxCommand;
/// # use std::time::Duration;
/// let cmd = BoxCommand::new("python3")
///     .args(["-c", "print('hello')"])
///     .env("PYTHONPATH", "/app")
///     .timeout(Duration::from_secs(30))
///     .working_dir("/workspace");
/// ```
#[derive(Clone, Debug)]
pub struct BoxCommand {
    pub(crate) command: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: Option<Vec<(String, String)>>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) working_dir: Option<String>,
    pub(crate) tty: bool,
}

impl BoxCommand {
    /// Create a new command.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: vec![],
            env: None,
            timeout: None,
            working_dir: None,
            tty: false,
        }
    }

    /// Add a single argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Add multiple arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable.
    pub fn env(mut self, key: impl Into<String>, val: impl Into<String>) -> Self {
        self.env
            .get_or_insert_with(Vec::new)
            .push((key.into(), val.into()));
        self
    }

    /// Set execution timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Set working directory.
    pub fn working_dir(mut self, dir: impl Into<String>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    /// Enable TTY (pseudo-terminal) for interactive sessions.
    ///
    /// Terminal size is auto-detected from the current terminal.
    pub fn tty(mut self, enable: bool) -> Self {
        self.tty = enable;
        self
    }
}

/// Handle to a running command execution.
///
/// Similar to `std::process::Child` but for remote execution in a guest.
/// Provides access to stdin, stdout, stderr streams and control operations.
///
/// # Examples
///
/// ```rust,no_run
/// # async fn example(litebox: &boxlite::LiteBox) -> Result<(), Box<dyn std::error::Error>> {
/// use boxlite::BoxCommand;
/// use futures::StreamExt;
///
/// let mut execution = litebox.exec(BoxCommand::new("ls").arg("-la")).await?;
///
/// // Read stdout
/// let mut stdout = execution.stdout.take().unwrap();
/// while let Some(line) = stdout.next().await {
///     println!("{}", line);
/// }
///
/// // Wait for completion
/// let status = execution.wait().await?;
/// println!("Exit code: {}", status.exit_code);
/// # Ok(())
/// # }
/// ```
pub struct Execution {
    id: ExecutionId,
    inner: std::sync::Arc<tokio::sync::Mutex<ExecutionInner>>,
}

pub(crate) struct ExecutionInner {
    interface: ExecutionInterface,
    result_rx: mpsc::UnboundedReceiver<ExecResult>,
    cached_result: Option<ExecResult>,

    /// Standard input stream (write-only).
    stdin: Option<ExecStdin>,

    /// Standard output stream (read-only).
    stdout: Option<ExecStdout>,

    /// Standard error stream (read-only).
    stderr: Option<ExecStderr>,
}

/// Unique identifier for an execution.
pub type ExecutionId = String;

impl Execution {
    /// Create a new Execution (internal use).
    pub(crate) fn new(
        execution_id: ExecutionId,
        interface: ExecutionInterface,
        result_rx: mpsc::UnboundedReceiver<ExecResult>,
        stdin: Option<ExecStdin>,
        stdout: Option<ExecStdout>,
        stderr: Option<ExecStderr>,
    ) -> Self {
        let inner = ExecutionInner {
            interface,
            result_rx,
            cached_result: None,
            stdin,
            stdout,
            stderr,
        };

        Self {
            id: execution_id,
            inner: std::sync::Arc::new(tokio::sync::Mutex::new(inner)),
        }
    }

    /// Get the execution ID.
    pub fn id(&self) -> &ExecutionId {
        &self.id
    }

    /// Take the stdin stream (can only be called once).
    pub fn stdin(&mut self) -> Option<ExecStdin> {
        futures::executor::block_on(async {
            let mut inner = self.inner.lock().await;
            inner.stdin.take()
        })
    }

    /// Take the stdout stream (can only be called once).
    pub fn stdout(&mut self) -> Option<ExecStdout> {
        futures::executor::block_on(async {
            let mut inner = self.inner.lock().await;
            inner.stdout.take()
        })
    }

    /// Take the stderr stream (can only be called once).
    pub fn stderr(&mut self) -> Option<ExecStderr> {
        futures::executor::block_on(async {
            let mut inner = self.inner.lock().await;
            inner.stderr.take()
        })
    }

    /// Wait for the execution to complete.
    ///
    /// Returns the exit status once the execution finishes. If the result is
    /// already cached, returns immediately. Otherwise, waits for result from channel.
    pub async fn wait(&mut self) -> BoxliteResult<ExecResult> {
        let mut inner = self.inner.lock().await;

        // Check if result is already cached
        if let Some(result) = &inner.cached_result {
            return Ok(result.clone());
        }

        // Try to receive from result channel (non-blocking)
        if let Ok(status) = inner.result_rx.try_recv() {
            inner.cached_result = Some(status.clone());
            return Ok(status);
        }

        // Await next result
        let status = inner.result_rx.recv().await.ok_or_else(|| {
            boxlite_shared::BoxliteError::Internal("Result channel closed".into())
        })?;
        inner.cached_result = Some(status.clone());
        Ok(status)
    }

    /// Kill the process (sends SIGKILL).
    pub async fn kill(&mut self) -> BoxliteResult<()> {
        self.signal(9).await // SIGKILL
    }

    /// Send a signal to the execution.
    pub async fn signal(&self, signal: i32) -> BoxliteResult<()> {
        let mut inner = self.inner.lock().await;
        inner.interface.kill(&self.id, signal).await
    }

    /// Resize PTY terminal window.
    ///
    /// Only works for executions started with TTY enabled.
    pub async fn resize_tty(&self, rows: u32, cols: u32) -> BoxliteResult<()> {
        let mut inner = self.inner.lock().await;
        inner.interface.resize_tty(&self.id, rows, cols, 0, 0).await
    }
}

/// Exit status of a process.
#[derive(Clone, Debug)]
pub struct ExecResult {
    /// Exit code (0 = success). If terminated by signal, code is negative signal number.
    pub exit_code: i32,
}

impl ExecResult {
    /// Returns true if the exit code was 0.
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    pub fn code(&self) -> i32 {
        self.exit_code
    }
}

/// Standard input stream (write-only).
pub struct ExecStdin {
    sender: mpsc::UnboundedSender<Vec<u8>>,
}

impl ExecStdin {
    pub(crate) fn new(sender: mpsc::UnboundedSender<Vec<u8>>) -> Self {
        Self { sender }
    }

    /// Write data to stdin.
    pub async fn write(&mut self, data: &[u8]) -> BoxliteResult<()> {
        self.sender
            .send(data.to_vec())
            .map_err(|_| boxlite_shared::BoxliteError::Internal("stdin channel closed".to_string()))
    }

    /// Write all data to stdin.
    pub async fn write_all(&mut self, data: &[u8]) -> BoxliteResult<()> {
        self.write(data).await
    }
}

/// Standard output stream (read-only).
pub struct ExecStdout {
    receiver: mpsc::UnboundedReceiver<String>,
}

impl ExecStdout {
    pub(crate) fn new(receiver: mpsc::UnboundedReceiver<String>) -> Self {
        Self { receiver }
    }
}

impl Stream for ExecStdout {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

/// Standard error stream (read-only).
pub struct ExecStderr {
    receiver: mpsc::UnboundedReceiver<String>,
}

impl ExecStderr {
    pub(crate) fn new(receiver: mpsc::UnboundedReceiver<String>) -> Self {
        Self { receiver }
    }
}

impl Stream for ExecStderr {
    type Item = String;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}
