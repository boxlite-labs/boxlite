//! Executor trait and implementations.
//!
//! Provides abstraction for spawning processes in different contexts:
//! - ContainerExecutor: runs commands inside OCI container
//! - GuestExecutor: runs commands directly on guest

use crate::container::Container;
use crate::service::exec::exec_handle::{ExecHandle, PtyConfig};
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use boxlite_shared::ExecRequest;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Executor spawns processes.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Spawn process from ExecRequest.
    async fn spawn(&self, req: &ExecRequest) -> BoxliteResult<ExecHandle>;
}

/// Executes commands inside OCI container.
pub struct ContainerExecutor {
    container: Arc<Mutex<Container>>,
}

impl ContainerExecutor {
    pub fn new(container: Arc<Mutex<Container>>) -> Self {
        Self { container }
    }
}

#[async_trait]
impl Executor for ContainerExecutor {
    async fn spawn(&self, req: &ExecRequest) -> BoxliteResult<ExecHandle> {
        let container = self.container.lock().await;

        let mut cmd = container
            .cmd()
            .program(&req.program)
            .args(&req.args)
            .envs(req.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

        if !req.workdir.is_empty() {
            cmd = cmd.current_dir(&req.workdir);
        }

        if let Some(tty) = &req.tty {
            cmd = cmd.with_pty(PtyConfig {
                rows: tty.rows as u16,
                cols: tty.cols as u16,
                x_pixels: tty.x_pixels as u16,
                y_pixels: tty.y_pixels as u16,
            });
        }

        cmd.spawn().await
    }
}

/// Executes commands directly on guest (no container).
pub struct GuestExecutor;

#[async_trait]
impl Executor for GuestExecutor {
    async fn spawn(&self, req: &ExecRequest) -> BoxliteResult<ExecHandle> {
        if let Some(tty) = &req.tty {
            let config = PtyConfig {
                rows: tty.rows as u16,
                cols: tty.cols as u16,
                x_pixels: tty.x_pixels as u16,
                y_pixels: tty.y_pixels as u16,
            };
            spawn_with_pty(req, config)
        } else {
            spawn_with_pipes(req)
        }
    }
}

/// Spawn process with pipes (standard mode).
fn spawn_with_pipes(req: &ExecRequest) -> BoxliteResult<ExecHandle> {
    use nix::unistd::Pid;
    use std::os::unix::io::{AsRawFd, FromRawFd};
    use tokio::process::Command;

    let mut cmd = Command::new(&req.program);
    cmd.args(&req.args);

    for (k, v) in &req.env {
        cmd.env(k, v);
    }

    if !req.workdir.is_empty() {
        cmd.current_dir(&req.workdir);
    }

    // Create pipes for stdin/stdout/stderr
    let (stdin_read, stdin_write) = nix::unistd::pipe()
        .map_err(|e| BoxliteError::Internal(format!("Failed to create stdin pipe: {}", e)))?;
    let (stdout_read, stdout_write) = nix::unistd::pipe()
        .map_err(|e| BoxliteError::Internal(format!("Failed to create stdout pipe: {}", e)))?;
    let (stderr_read, stderr_write) = nix::unistd::pipe()
        .map_err(|e| BoxliteError::Internal(format!("Failed to create stderr pipe: {}", e)))?;

    // Configure command to use our pipes
    unsafe {
        cmd.stdin(std::process::Stdio::from_raw_fd(stdin_read.as_raw_fd()));
        cmd.stdout(std::process::Stdio::from_raw_fd(stdout_write.as_raw_fd()));
        cmd.stderr(std::process::Stdio::from_raw_fd(stderr_write.as_raw_fd()));
    }

    let child = cmd
        .spawn()
        .map_err(|e| BoxliteError::Internal(format!("Failed to spawn '{}': {}", req.program, e)))?;

    let pid = child
        .id()
        .ok_or_else(|| BoxliteError::Internal("Process exited immediately".into()))?;

    // Close the read end of stdin and write ends of stdout/stderr in parent
    drop(stdin_read);
    drop(stdout_write);
    drop(stderr_write);

    // Non-PTY mode: stdout and stderr are separate pipes
    Ok(ExecHandle::new(
        Pid::from_raw(pid as i32),
        stdin_write,
        stdout_read,
        Some(stderr_read),
    ))
}

/// Spawn process with PTY (interactive mode).
fn spawn_with_pty(req: &ExecRequest, config: PtyConfig) -> BoxliteResult<ExecHandle> {
    use nix::pty::{openpty, OpenptyResult, Winsize};
    use nix::unistd::{dup, Pid};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    // Create PTY pair
    let winsize = Winsize {
        ws_row: config.rows,
        ws_col: config.cols,
        ws_xpixel: config.x_pixels,
        ws_ypixel: config.y_pixels,
    };

    let OpenptyResult { master, slave } = openpty(Some(&winsize), None)
        .map_err(|e| BoxliteError::Internal(format!("Failed to create PTY: {}", e)))?;

    // Get raw FD for use in pre_exec closure
    let slave_raw_fd = slave.as_raw_fd();

    // Duplicate slave FD for each stdio (Stdio::from_raw_fd takes ownership and will close)
    let slave_stdin = dup(slave.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup slave for stdin: {}", e)))?;
    let slave_stdout = dup(slave.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup slave for stdout: {}", e)))?;
    let slave_stderr = dup(slave.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup slave for stderr: {}", e)))?;

    // Build command
    let mut cmd = Command::new(&req.program);
    cmd.args(&req.args);

    for (k, v) in &req.env {
        cmd.env(k, v);
    }

    if !req.workdir.is_empty() {
        cmd.current_dir(&req.workdir);
    }

    // Configure child to use PTY slave as stdin/stdout/stderr
    // Each Stdio takes ownership of its dup'd FD
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(slave_stdin));
        cmd.stdout(Stdio::from_raw_fd(slave_stdout));
        cmd.stderr(Stdio::from_raw_fd(slave_stderr));
    }

    // Set up session and controlling terminal in child
    unsafe {
        cmd.pre_exec(move || {
            // Create new session (detach from parent's controlling terminal)
            nix::unistd::setsid().map_err(std::io::Error::other)?;

            // Set the PTY slave as the controlling terminal
            if nix::libc::ioctl(slave_raw_fd, nix::libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(())
        });
    }

    let child = cmd
        .spawn()
        .map_err(|e| BoxliteError::Internal(format!("Failed to spawn '{}': {}", req.program, e)))?;

    let pid = child.id();

    // Close original slave in parent (child has its dup'd copies after fork)
    drop(slave);

    // Duplicate master FD for stdin and stdout only.
    // In PTY mode, stderr is merged into stdout at the PTY level - there is only
    // ONE reader from the PTY master. Creating separate readers would cause a race
    // condition where data could be captured by the "wrong" reader, resulting in
    // out-of-order output.
    let stdin_fd = dup(master.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup PTY for stdin: {}", e)))?;
    let stdout_fd = dup(master.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup PTY for stdout: {}", e)))?;

    let stdin = unsafe { OwnedFd::from_raw_fd(stdin_fd) };
    let stdout = unsafe { OwnedFd::from_raw_fd(stdout_fd) };

    // PTY mode: stderr is None (merged into stdout)
    let mut handle = ExecHandle::new(Pid::from_raw(pid as i32), stdin, stdout, None);

    // Keep master FD for resize operations
    let pty_controller = {
        let fd = master.as_raw_fd();
        std::mem::forget(master); // Transfer ownership
        unsafe { std::fs::File::from_raw_fd(fd) }
    };
    handle.set_pty(pty_controller, config);

    Ok(handle)
}
