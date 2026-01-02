//! Process execution handle
//!
//! Provides types for managing a running process.
//! Works for both container and direct guest execution.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use futures::stream::{Stream, StreamExt};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use std::os::unix::io::OwnedFd;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::AsyncWriteExt;

/// Stdin writer for executed process
///
/// Async wrapper around file descriptor for writing to process stdin.
pub struct ExecStdin {
    inner: tokio::fs::File,
}

impl ExecStdin {
    /// Create from file descriptor
    pub fn new(fd: OwnedFd) -> Self {
        use std::os::fd::{FromRawFd, IntoRawFd};
        let std_file = unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) };
        Self {
            inner: tokio::fs::File::from_std(std_file),
        }
    }

    /// Write all data to stdin
    ///
    /// # Errors
    ///
    /// - I/O error (pipe closed, etc.)
    pub async fn write_all(&mut self, data: &[u8]) -> BoxliteResult<()> {
        self.inner
            .write_all(data)
            .await
            .map_err(|e| BoxliteError::Internal(format!("Failed to write to stdin: {}", e)))
    }
}

// Shared output stream implementation
struct OutputStream {
    inner: Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>,
}

impl OutputStream {
    fn new(fd: OwnedFd) -> Self {
        use async_stream::stream;
        use std::os::fd::{FromRawFd, IntoRawFd};
        use tokio::io::AsyncReadExt;

        // Convert OwnedFd to tokio file
        let std_file = unsafe { std::fs::File::from_raw_fd(fd.into_raw_fd()) };
        let file = tokio::fs::File::from_std(std_file);
        let mut reader = tokio::io::BufReader::new(file);

        // Read chunks as they arrive (works for both PTY and pipes)
        let stream = stream! {
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,  // EOF
                    Ok(n) => yield buf[..n].to_vec(),
                    Err(_) => break,
                }
            }
        };

        Self {
            inner: Box::pin(stream),
        }
    }
}

impl Stream for OutputStream {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// Stdout stream from executed process
///
/// Stream that yields lines from stdout.
pub struct ExecStdout {
    inner: OutputStream,
}

impl ExecStdout {
    /// Create from file descriptor
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            inner: OutputStream::new(fd),
        }
    }
}

impl Stream for ExecStdout {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

/// Stderr stream from executed process
///
/// Stream that yields lines from stderr.
pub struct ExecStderr {
    inner: OutputStream,
}

impl ExecStderr {
    /// Create from file descriptor
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            inner: OutputStream::new(fd),
        }
    }
}

impl Stream for ExecStderr {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

/// Process exit status
///
/// Either normal exit with code or termination by signal.
#[derive(Debug, Clone, Copy)]
pub enum ExitStatus {
    /// Process exited normally with exit code
    Code(i32),

    /// Process was terminated by signal
    #[allow(dead_code)] // API completeness for future signal handling
    Signal(Signal),
}

impl ExitStatus {
    /// Get exit code
    ///
    /// Returns the exit code for normal termination, or 0 if killed by signal.
    #[allow(dead_code)] // API completeness for std::process::ExitStatus compatibility
    pub fn code(&self) -> i32 {
        match self {
            ExitStatus::Code(c) => *c,
            ExitStatus::Signal(_) => 0,
        }
    }

    /// Check if process exited successfully
    ///
    /// Returns `true` if exit code is 0, `false` otherwise (including signals).
    #[allow(dead_code)] // API completeness for std::process::ExitStatus compatibility
    pub fn success(&self) -> bool {
        matches!(self, ExitStatus::Code(0))
    }
}

/// PTY configuration
#[derive(Clone, Debug)]
pub struct PtyConfig {
    pub rows: u16,
    pub cols: u16,
    pub x_pixels: u16,
    pub y_pixels: u16,
}

/// Handle to a running process
///
/// Represents a spawned process with stdin, stdout, and stderr.
/// Works for both container execution and direct guest execution.
///
/// # Example
///
/// ```no_run
/// # use guest::execution::*;
/// # use futures::StreamExt;
/// # async fn example(mut handle: ExecHandle) -> Result<(), Box<dyn std::error::Error>> {
/// // Write to stdin
/// if let Some(mut stdin) = handle.stdin() {
///     stdin.write_all(b"hello\n").await?;
/// }
///
/// // Read stdout and stderr separately
/// let mut stdout = handle.stdout().unwrap();
/// let mut stderr = handle.stderr().unwrap();
///
/// // Stream stdout
/// tokio::spawn(async move {
///     while let Some(line) = stdout.next().await {
///         println!("out: {}", String::from_utf8_lossy(&line));
///     }
/// });
///
/// // Stream stderr
/// tokio::spawn(async move {
///     while let Some(line) = stderr.next().await {
///         eprintln!("err: {}", String::from_utf8_lossy(&line));
///     }
/// });
///
/// // Wait for exit
/// let status = handle.wait().await?;
/// assert!(status.success());
/// # Ok(())
/// # }
/// ```
pub struct ExecHandle {
    /// Process ID
    pid: Pid,

    /// Stdin writer (None if closed)
    stdin: Option<ExecStdin>,

    /// Stdout stream
    stdout: Option<ExecStdout>,

    /// Stderr stream
    stderr: Option<ExecStderr>,

    /// PTY controller (for resize)
    ///
    /// Only present when spawned with PTY mode (tty=true).
    /// Used for terminal size operations via ioctl.
    pty_controller: Option<std::fs::File>,

    /// PTY configuration
    ///
    /// Only present when spawned with PTY mode (tty=true).
    pty_config: Option<PtyConfig>,
}

impl ExecHandle {
    /// Create new execution handle.
    ///
    /// # Arguments
    ///
    /// * `pid` - Process ID
    /// * `stdin` - Stdin file descriptor
    /// * `stdout` - Stdout file descriptor
    /// * `stderr` - Stderr file descriptor, or `None` in PTY mode (merged into stdout)
    pub fn new(pid: Pid, stdin: OwnedFd, stdout: OwnedFd, stderr: Option<OwnedFd>) -> Self {
        Self {
            pid,
            stdin: Some(ExecStdin::new(stdin)),
            stdout: Some(ExecStdout::new(stdout)),
            // In PTY mode, stderr is None because stdout/stderr are merged
            // at the PTY level (single reader from PTY master)
            stderr: stderr.map(ExecStderr::new),
            pty_controller: None,
            pty_config: None,
        }
    }

    /// Set PTY controller and config
    ///
    /// Called when process is spawned with console socket (PTY mode).
    pub fn set_pty(&mut self, controller: std::fs::File, config: PtyConfig) {
        self.pty_controller = Some(controller);
        self.pty_config = Some(config);
    }

    /// Get PTY controller for resize operations
    pub fn pty_controller(&self) -> Option<&std::fs::File> {
        self.pty_controller.as_ref()
    }

    /// Get PTY config
    #[allow(dead_code)] // API completeness
    pub fn pty_config(&self) -> Option<&PtyConfig> {
        self.pty_config.as_ref()
    }

    /// Get process ID
    pub fn pid(&self) -> Pid {
        self.pid
    }

    /// Take stdin writer
    ///
    /// Returns `None` if stdin was already taken.
    pub fn stdin(&mut self) -> Option<ExecStdin> {
        self.stdin.take()
    }

    /// Close stdin (signals EOF to process)
    ///
    /// This drops the stdin handle, preventing further writes.
    /// The underlying file descriptor is closed when dropped.
    ///
    /// # Idempotent
    ///
    /// Safe to call multiple times (no-op if already closed).
    #[allow(dead_code)] // API completeness
    pub fn close_stdin(&mut self) {
        self.stdin = None; // Drop closes the fd
    }

    /// Take stdout stream
    ///
    /// Returns the stdout stream. After calling this, you cannot call it again.
    /// The stream can be moved into a spawned task for concurrent reading.
    pub fn stdout(&mut self) -> Option<ExecStdout> {
        self.stdout.take()
    }

    /// Take stderr stream
    ///
    /// Returns the stderr stream. After calling this, you cannot call it again.
    /// The stream can be moved into a spawned task for concurrent reading.
    pub fn stderr(&mut self) -> Option<ExecStderr> {
        self.stderr.take()
    }

    /// Wait for process to complete
    ///
    /// This consumes the handle and blocks until the process exits.
    ///
    /// # Returns
    ///
    /// - `ExitStatus::Code(n)` if process exited normally
    /// - `ExitStatus::Signal(sig)` if process was terminated by signal
    #[allow(dead_code)] // API completeness
    pub async fn wait(&self) -> BoxliteResult<ExitStatus> {
        use nix::sys::wait::{waitpid, WaitStatus};

        match waitpid(self.pid, None).map_err(|e| {
            BoxliteError::Internal(format!("Failed to wait for process {}: {}", self.pid, e))
        })? {
            WaitStatus::Exited(_, code) => Ok(ExitStatus::Code(code)),
            WaitStatus::Signaled(_, signal, _) => Ok(ExitStatus::Signal(signal)),
            other => Err(BoxliteError::Internal(format!(
                "Process {} exited with unexpected status: {:?}",
                self.pid, other
            ))),
        }
    }

    /// Kill process with signal
    ///
    /// Sends a signal to the process. Does not consume the handle,
    /// so you can still wait() afterward.
    ///
    /// # Arguments
    ///
    /// - `signal`: POSIX signal number (9 = SIGKILL, 15 = SIGTERM, etc.)
    ///
    /// # Errors
    ///
    /// - Invalid signal number
    /// - Process already exited
    /// - Permission denied
    pub fn kill(&self, signal: Signal) -> BoxliteResult<()> {
        use nix::sys::signal::kill;

        kill(self.pid, signal).map_err(|e| {
            BoxliteError::Internal(format!(
                "Failed to send signal {} to process {}: {}",
                signal, self.pid, e
            ))
        })
    }
}
