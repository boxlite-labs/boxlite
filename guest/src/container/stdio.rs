//! Container init process stdio management.
//!
//! Provides pipe-based stdio that keeps init processes alive by holding
//! the write-end of stdin open (never written to, never closed).
//!
//! # Problem
//!
//! When container init's stdin is /dev/null, interactive entrypoints like
//! `/bin/sh` or `python` detect EOF and exit immediately, invalidating
//! the container namespace for subsequent exec operations.
//!
//! # Solution
//!
//! Create pipes where boxlite-guest holds the write-end of stdin open.
//! The init process blocks on `read(stdin)` indefinitely.
//!
//! # Example
//!
//! ```ignore
//! let (stdio, init_fds) = ContainerStdio::new()?;
//!
//! // Pass init_fds to libcontainer
//! ContainerBuilder::new(...)
//!     .with_stdin(init_fds.stdin)
//!     .with_stdout(init_fds.stdout)
//!     .with_stderr(init_fds.stderr)
//!     .build()?;
//!
//! // Hold stdio in Container struct - init blocks forever
//! let container = Container { stdio, ... };
//!
//! // When container is dropped, stdio is dropped → init gets EOF → exits
//! ```

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use nix::unistd::pipe;
use std::os::unix::io::OwnedFd;

/// Stdio configuration for container init process.
///
/// Holds pipe file descriptors:
/// - stdin_tx: write-end held open (blocks init's read forever)
/// - stdout_rx/stderr_rx: read-ends for optional log capture
///
/// # Lifecycle
///
/// 1. Create pipes before container start
/// 2. Pass read-end of stdin to container init via `InitStdioFds`
/// 3. Hold write-end in ContainerStdio (never write, never close)
/// 4. Init process blocks on read(stdin) indefinitely
/// 5. On container stop, drop ContainerStdio → pipes close → init gets EOF
#[derive(Debug)]
pub struct ContainerStdio {
    /// Write-end of stdin pipe (held open, never written to)
    #[allow(dead_code)]
    stdin_tx: OwnedFd,

    /// Read-end of stdout pipe (for optional log capture)
    #[allow(dead_code)]
    stdout_rx: OwnedFd,

    /// Read-end of stderr pipe (for optional log capture)
    #[allow(dead_code)]
    stderr_rx: OwnedFd,
}

/// File descriptors to pass to container init process.
///
/// These are the "child side" of the pipes:
/// - stdin: read-end (init reads from this, blocks when empty)
/// - stdout: write-end (init writes here)
/// - stderr: write-end (init writes here)
///
/// Pass these to libcontainer's `ContainerBuilder::with_stdin/stdout/stderr`.
#[derive(Debug)]
pub struct InitStdioFds {
    /// Read-end of stdin pipe (init reads from this)
    pub stdin: OwnedFd,

    /// Write-end of stdout pipe (init writes here)
    pub stdout: OwnedFd,

    /// Write-end of stderr pipe (init writes here)
    pub stderr: OwnedFd,
}

impl ContainerStdio {
    /// Create new stdio pipes for container init.
    ///
    /// Returns `(ContainerStdio, InitStdioFds)` where:
    /// - `ContainerStdio`: held by boxlite-guest to keep init alive
    /// - `InitStdioFds`: passed to libcontainer for init process
    ///
    /// # Errors
    ///
    /// Returns error if pipe creation fails.
    pub fn new() -> BoxliteResult<(Self, InitStdioFds)> {
        // Create stdin pipe: init reads from rx, we hold tx open
        let (stdin_rx, stdin_tx) = pipe()
            .map_err(|e| BoxliteError::Internal(format!("Failed to create stdin pipe: {}", e)))?;

        // Create stdout pipe: init writes to tx, we can read from rx
        let (stdout_rx, stdout_tx) = pipe()
            .map_err(|e| BoxliteError::Internal(format!("Failed to create stdout pipe: {}", e)))?;

        // Create stderr pipe: init writes to tx, we can read from rx
        let (stderr_rx, stderr_tx) = pipe()
            .map_err(|e| BoxliteError::Internal(format!("Failed to create stderr pipe: {}", e)))?;

        // nix::unistd::pipe() returns OwnedFd directly
        let container_stdio = Self {
            stdin_tx,
            stdout_rx,
            stderr_rx,
        };

        let init_fds = InitStdioFds {
            stdin: stdin_rx,
            stdout: stdout_tx,
            stderr: stderr_tx,
        };

        tracing::debug!("Created container stdio pipes");

        Ok((container_stdio, init_fds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn test_stdio_creation() {
        let result = ContainerStdio::new();
        assert!(result.is_ok());

        let (stdio, init_fds) = result.unwrap();

        // Verify all FDs are valid (positive integers)
        assert!(stdio.stdin_tx.as_raw_fd() >= 0);
        assert!(stdio.stdout_rx.as_raw_fd() >= 0);
        assert!(stdio.stderr_rx.as_raw_fd() >= 0);
        assert!(init_fds.stdin.as_raw_fd() >= 0);
        assert!(init_fds.stdout.as_raw_fd() >= 0);
        assert!(init_fds.stderr.as_raw_fd() >= 0);

        // Verify all FDs are unique
        let fds = [
            stdio.stdin_tx.as_raw_fd(),
            stdio.stdout_rx.as_raw_fd(),
            stdio.stderr_rx.as_raw_fd(),
            init_fds.stdin.as_raw_fd(),
            init_fds.stdout.as_raw_fd(),
            init_fds.stderr.as_raw_fd(),
        ];
        for i in 0..fds.len() {
            for j in (i + 1)..fds.len() {
                assert_ne!(fds[i], fds[j], "FDs should be unique");
            }
        }
    }
}
