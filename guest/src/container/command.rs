//! Command builder for executing processes in containers
//!
//! Provides a builder pattern for spawning processes inside containers,
//! following the `std::process::Command` pattern.

use super::capabilities::capability_names;
use crate::service::exec::exec_handle::{ExecHandle, PtyConfig};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::syscall::syscall::SyscallType;
use nix::unistd::Pid;
use std::collections::HashMap;
use std::os::unix::io::OwnedFd;
use std::path::PathBuf;

/// Command builder
///
/// Builds a command to execute inside a container with stdin/stdout/stderr.
/// Use the builder methods to configure the command, arguments, environment, and working directory.
///
/// # Example
///
/// ```no_run
/// # use guest::container::Container;
/// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
/// let mut child = container
///     .cmd()
///     .program("ls")
///     .args(&["-la", "/tmp"])
///     .env("FOO", "bar")
///     .current_dir("/home")
///     .spawn()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct ContainerCommand {
    // Container context (provided by Container::exec())
    id: String,

    state_root: PathBuf,

    /// Program to execute (set via cmd())
    program: Option<String>,

    /// Command arguments (not including program)
    args: Vec<String>,

    /// Environment variable overrides
    env: HashMap<String, String>,

    /// Working directory (None = use default "/")
    cwd: Option<String>,

    /// Console socket path for PTY (internal, set by spawn when pty_config is present)
    console_socket: Option<String>,

    /// PTY configuration (set via with_pty())
    pty_config: Option<PtyConfig>,
}

impl ContainerCommand {
    /// Create new command builder
    ///
    /// This is public within the crate for use by Container::exec().
    /// Users should call `container.exec()` instead.
    pub(super) fn new(id: String, state_root: PathBuf, env: HashMap<String, String>) -> Self {
        Self {
            program: None,
            args: Vec::new(),
            env,
            cwd: None,
            console_socket: None,
            pty_config: None,
            id,
            state_root,
        }
    }

    /// Enable PTY mode with configuration
    ///
    /// Sets up console socket for OCI-compliant PTY handling.
    /// Call this before spawn() to enable PTY mode.
    pub fn with_pty(mut self, config: PtyConfig) -> Self {
        // Store config for spawn() to use
        self.pty_config = Some(config);
        self
    }

    /// Set the program to execute
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let child = container.exec().cmd("ls").spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn program(mut self, program: impl Into<String>) -> Self {
        self.program = Some(program.into());
        self
    }

    /// Add arguments (replaces existing)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let child = container.command("ls").args(&["-la", "/tmp"]).spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.args = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        self
    }

    /// Add single argument
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let child = container.command("ls").arg("-l").arg("-a").spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[allow(dead_code)] // API completeness for std::process::Command compatibility
    pub fn arg(mut self, arg: impl AsRef<str>) -> Self {
        self.args.push(arg.as_ref().to_string());
        self
    }

    /// Set environment variable
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let child = container.command("env").env("FOO", "bar").spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[allow(dead_code)] // API completeness for std::process::Command compatibility
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set multiple environment variables
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let vars = vec![("FOO", "bar"), ("BAZ", "qux")];
    /// let child = container.command("env").envs(vars).spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in vars {
            self.env.insert(k.into(), v.into());
        }
        self
    }

    /// Set working directory
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let child = container.command("pwd").current_dir("/tmp").spawn().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[allow(dead_code)] // API completeness for std::process::Command compatibility
    pub fn current_dir(mut self, dir: impl Into<String>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Spawn the process
    ///
    /// Creates a tenant process in the container with stdin/stdout/stderr pipes.
    /// Returns an [`ExecHandle`] for interacting with the running process.
    ///
    /// # Errors
    ///
    /// - No program specified (must call `.cmd()` first)
    /// - Failed to create pipes
    /// - Failed to spawn process
    /// - Invalid container state
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # use futures::StreamExt;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut child = container.exec().cmd("sh").arg("-c").arg("echo hello").spawn().await?;
    ///
    /// // Read output
    /// while let Some(chunk) = child.output().next().await {
    ///     println!("{}", String::from_utf8_lossy(&chunk.data));
    /// }
    ///
    /// // Wait for exit
    /// let status = child.wait().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn spawn(self) -> BoxliteResult<ExecHandle> {
        if let Some(pty_config) = self.pty_config.clone() {
            self.spawn_with_pty(pty_config).await
        } else {
            self.spawn_with_pipes().await
        }
    }

    /// Spawn process with pipes (standard mode).
    async fn spawn_with_pipes(self) -> BoxliteResult<ExecHandle> {
        use nix::unistd::pipe;

        // Create pipes for I/O
        let (stdin_read, stdin_write) = pipe()
            .map_err(|e| BoxliteError::Internal(format!("Failed to create stdin pipe: {}", e)))?;
        let (stdout_read, stdout_write) = pipe()
            .map_err(|e| BoxliteError::Internal(format!("Failed to create stdout pipe: {}", e)))?;
        let (stderr_read, stderr_write) = pipe()
            .map_err(|e| BoxliteError::Internal(format!("Failed to create stderr pipe: {}", e)))?;

        tracing::debug!(container_id = %self.id, "Spawning with pipes");

        let pipes = Some((stdin_read, stdout_write, stderr_write));
        let pid = self.build_and_spawn(pipes).await?;

        tracing::debug!(pid = pid.as_raw(), "Spawned with pipes");
        Ok(ExecHandle::new(pid, stdin_write, stdout_read, stderr_read))
    }

    /// Spawn process with PTY (interactive mode).
    async fn spawn_with_pty(mut self, config: PtyConfig) -> BoxliteResult<ExecHandle> {
        use super::console_socket::ConsoleSocket;

        // Setup console socket
        let exec_id = uuid::Uuid::new_v4().to_string();
        let socket = ConsoleSocket::new(&exec_id)?;

        tracing::debug!(
            container_id = %self.id,
            console_socket = %socket.path(),
            "Spawning with PTY"
        );

        // Spawn process with console socket
        self.console_socket = Some(socket.path().to_string());
        let pid = self.build_and_spawn(None).await?;

        // Receive PTY master FD (socket auto-cleanup on drop)
        let pty_master = socket.receive_pty_master()?;

        // Create child with PTY
        create_pty_child(pid, pty_master, config)
    }

    /// Build and spawn process using libcontainer.
    async fn build_and_spawn(
        &self,
        pipes: Option<(OwnedFd, OwnedFd, OwnedFd)>,
    ) -> BoxliteResult<Pid> {
        // Build command arguments
        let program = self.program.clone().unwrap_or("".into());
        let mut container_args = vec![program.clone()];
        container_args.extend_from_slice(self.args.as_slice());

        // Build container
        let mut builder = ContainerBuilder::new(self.id.to_string(), SyscallType::default())
            .with_root_path(self.state_root.clone())
            .map_err(|e| {
                BoxliteError::Internal(format!("Failed to set container root path: {}", e))
            })?
            .with_console_socket(self.console_socket.clone())
            .validate_id()
            .map_err(|e| BoxliteError::Internal(format!("Invalid container ID: {}", e)))?;

        // Add pipes if provided
        if let Some((stdin, stdout, stderr)) = pipes {
            builder = builder
                .with_stdin(stdin)
                .with_stdout(stdout)
                .with_stderr(stderr);
        }

        // Configure and spawn
        tracing::debug!(
            container_id = %self.id,
            state_root = %self.state_root.display(),
            program = %program,
            args = ?container_args,
            "About to call libcontainer build() to exec into container"
        );

        // Check container status before attempting exec
        let container_state_path = self.state_root.join(&self.id);
        if let Ok(container) =
            libcontainer::container::Container::load(container_state_path.clone())
        {
            tracing::debug!(
                container_id = %self.id,
                status = ?container.status(),
                state_path = %container_state_path.display(),
                "Container status before exec"
            );
        } else {
            tracing::warn!(
                container_id = %self.id,
                state_path = %container_state_path.display(),
                "Failed to load container status before exec"
            );
        }

        let pid = builder
            .as_tenant()
            .with_capabilities(capability_names())
            .with_no_new_privs(false)
            .with_detach(false)
            .with_cwd(self.cwd.clone().or(Some("/".parse().unwrap())))
            .with_env(self.env.clone())
            .with_container_args(container_args.clone())
            .build()
            .map_err(|e| {
                tracing::error!(
                    container_id = %self.id,
                    program = %program,
                    args = ?container_args,
                    error = %e,
                    state_root = %self.state_root.display(),
                    "Libcontainer build() failed - likely container status issue"
                );

                // Try to get container status after failure
                let container_state_path = self.state_root.join(&self.id);
                if let Ok(container) =
                    libcontainer::container::Container::load(container_state_path.clone())
                {
                    tracing::error!(
                        container_id = %self.id,
                        status = ?container.status(),
                        "Container status after exec failure"
                    );
                }

                BoxliteError::Internal(format!(
                    "Failed to spawn '{}' with args {:?}: {}",
                    program, container_args, e
                ))
            })?;

        tracing::debug!(
            container_id = %self.id,
            pid = pid.as_raw(),
            "Successfully spawned process in container"
        );

        Ok(pid)
    }
}

/// Create ExecHandle with PTY.
///
/// Sets terminal window size, reconciles PTY master FD as stdin/stdout/stderr,
/// and stores PTY controller for later resizing.
fn create_pty_child(pid: Pid, pty_master: OwnedFd, config: PtyConfig) -> BoxliteResult<ExecHandle> {
    set_pty_window_size(&pty_master, &config)?;
    let (stdin, stdout, stderr) = reconcile_pty_fds(&pty_master)?;

    let mut child = ExecHandle::new(pid, stdin, stdout, stderr);
    let pty_controller = pty_master_to_file(pty_master);
    child.set_pty(pty_controller, config);

    Ok(child)
}

/// Set PTY terminal window size via ioctl.
fn set_pty_window_size(pty_master: &OwnedFd, config: &PtyConfig) -> BoxliteResult<()> {
    use nix::pty::Winsize;
    use std::os::fd::AsRawFd;

    let winsize = Winsize {
        ws_row: config.rows,
        ws_col: config.cols,
        ws_xpixel: config.x_pixels,
        ws_ypixel: config.y_pixels,
    };

    unsafe {
        if nix::libc::ioctl(
            pty_master.as_raw_fd(),
            nix::libc::TIOCSWINSZ,
            &winsize as *const _,
        ) == -1
        {
            let errno = std::io::Error::last_os_error();
            return Err(BoxliteError::Internal(format!(
                "Failed to set PTY window size ({}x{}): {}",
                config.rows, config.cols, errno
            )));
        }
    }

    Ok(())
}

/// Reconcile PTY master FD as stdin/stdout/stderr.
///
/// Duplicates the PTY master FD three times so it can be used as separate
/// stdin, stdout, and stderr streams. This allows reusing existing pipe-based
/// I/O forwarding code.
fn reconcile_pty_fds(pty_master: &OwnedFd) -> BoxliteResult<(OwnedFd, OwnedFd, OwnedFd)> {
    use nix::unistd::dup;
    use std::os::fd::{AsRawFd, FromRawFd};

    let stdin_fd = dup(pty_master.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup PTY for stdin: {}", e)))?;
    let stdout_fd = dup(pty_master.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup PTY for stdout: {}", e)))?;
    let stderr_fd = dup(pty_master.as_raw_fd())
        .map_err(|e| BoxliteError::Internal(format!("Failed to dup PTY for stderr: {}", e)))?;

    Ok((
        unsafe { OwnedFd::from_raw_fd(stdin_fd) },
        unsafe { OwnedFd::from_raw_fd(stdout_fd) },
        unsafe { OwnedFd::from_raw_fd(stderr_fd) },
    ))
}

/// Convert OwnedFd to File for PTY controller.
///
/// The PTY controller is kept for later resizing operations.
fn pty_master_to_file(pty_master: OwnedFd) -> std::fs::File {
    use std::os::fd::{AsRawFd, FromRawFd};

    let fd = pty_master.as_raw_fd();
    std::mem::forget(pty_master); // Transfer ownership, don't close
    unsafe { std::fs::File::from_raw_fd(fd) }
}
