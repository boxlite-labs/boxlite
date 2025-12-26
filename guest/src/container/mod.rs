//! OCI container runtime
//!
//! This module provides an OCI-compliant container runtime using libcontainer.
//!
//! # Architecture
//!
//! - [`Container`]: OCI container lifecycle (create, start, check status)
//! - [`ContainerCommand`]: Builder for executing commands inside container
//! - [`crate::service::exec::exec_handle::ExecHandle`]: Handle to a running process
//!
//! # Example
//!
//! ```no_run
//! use guest::container::Container;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Create and start container
//! let container = Container::start(
//!     "/rootfs",
//!     vec!["sh".to_string()],
//!     vec!["PATH=/bin:/usr/bin".to_string()],
//!     "/",
//! )?;
//!
//! // Execute command
//! let mut child = container
//!     .exec()
//!     .cmd("ls")
//!     .args(&["-la", "/tmp"])
//!     .env("FOO", "bar")
//!     .spawn()
//!     .await?;
//!
//! // Write to stdin
//! if let Some(stdin) = child.stdin() {
//!     stdin.write_all(b"hello\n").await?;
//! }
//!
//! // Stream stdout and stderr separately
//! use futures::StreamExt;
//! let stdout = child.stdout();
//! let stderr = child.stderr();
//!
//! // Spawn separate tasks for reading
//! tokio::spawn(async move {
//!     while let Some(line) = stdout.next().await {
//!         println!("out: {}", String::from_utf8_lossy(&line));
//!     }
//! });
//!
//! tokio::spawn(async move {
//!     while let Some(line) = stderr.next().await {
//!         eprintln!("err: {}", String::from_utf8_lossy(&line));
//!     }
//! });
//!
//! // Wait for exit
//! let status = child.wait().await?;
//! println!("Exit code: {}", status.code());
//! # Ok(())
//! # }
//! ```

#[cfg(target_os = "linux")]
mod capabilities;
#[cfg(target_os = "linux")]
mod command;
#[cfg(target_os = "linux")]
mod console_socket;
#[cfg(target_os = "linux")]
mod kill;
#[cfg(target_os = "linux")]
mod lifecycle;
#[cfg(target_os = "linux")]
mod spec;
#[cfg(target_os = "linux")]
mod start;
#[cfg(target_os = "linux")]
mod stdio;

#[cfg(target_os = "linux")]
pub use lifecycle::Container;
#[cfg(target_os = "linux")]
pub use spec::UserMount;
