//! OCI container lifecycle management
//!
//! Provides container creation, startup, and status checking using libcontainer.
//! Follows the OCI Runtime Specification.

use super::command::ContainerCommand;
use super::{kill, start};
use crate::layout::GuestLayout;
use boxlite_shared::errors::BoxliteResult;
use libcontainer::container::Container as LibContainer;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// OCI container
///
/// Manages the lifecycle of an OCI-compliant container using libcontainer.
/// Follows the OCI Runtime Specification.
///
/// # Example
///
/// ```no_run
/// # use guest::container::Container;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create and start container
/// let container = Container::start(
///     "my-container",
///     "/rootfs",
///     vec!["sh".to_string()],
///     vec!["PATH=/bin:/usr/bin".to_string()],
///     "/",
/// )?;
///
/// // Execute command
/// let mut child = container.command("ls").args(&["-la"]).spawn().await?;
/// let status = child.wait().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Container {
    id: String,
    state_root: PathBuf,
    bundle_path: PathBuf,
    env: HashMap<String, String>,
}

impl Container {
    /// Create and start an OCI container
    ///
    /// Creates a container with the specified rootfs and starts the init process.
    /// The init process runs detached in the background.
    ///
    /// Uses GuestLayout internally to determine paths:
    /// - Container directory: /run/boxlite/{container_id}/
    /// - OCI bundle (config.json): /run/boxlite/{container_id}/config.json
    /// - libcontainer state: /run/boxlite/{container_id}/state.json
    ///
    /// # Arguments
    ///
    /// - `container_id`: Unique container identifier
    /// - `rootfs`: Path to container root filesystem
    /// - `entrypoint`: Command and arguments for container init process
    /// - `env`: Environment variables in "KEY=VALUE" format
    /// - `workdir`: Working directory inside container
    ///
    /// # Errors
    ///
    /// - Empty rootfs or entrypoint
    /// - Failed to create container directory
    /// - Failed to create or start container
    /// - Init process exited immediately
    pub fn start(
        container_id: &str,
        rootfs: impl AsRef<Path>,
        entrypoint: Vec<String>,
        env: Vec<String>,
        workdir: impl AsRef<Path>,
    ) -> BoxliteResult<Self> {
        let rootfs = rootfs.as_ref();
        let workdir = workdir.as_ref();

        // Use GuestLayout for all paths (per-container directories)
        let layout = GuestLayout::new();

        // Validate inputs early
        start::validate_container_inputs(rootfs, &entrypoint, workdir)?;

        // Parse existing env into map (KEY=VALUE)
        let mut env_map: HashMap<String, String> = HashMap::new();
        for entry in &env {
            if let Some(pos) = entry.find('=') {
                let key = entry[..pos].to_string();
                let value = entry[pos + 1..].to_string();
                env_map.insert(key, value);
            }
        }

        // State at /run/boxlite/containers/{cid}/state/
        let state_root = layout.container_state_dir(container_id);

        // Create OCI bundle at /run/boxlite/containers/{cid}/
        // create_oci_bundle creates bundle_root/{cid}/, so pass containers_dir
        let bundle_path = start::create_oci_bundle(
            container_id,
            rootfs,
            &entrypoint,
            &env,
            workdir,
            &layout.containers_dir(),
        )?;

        // Create and start container
        start::create_container(container_id, &state_root, &bundle_path)?;
        start::start_container(container_id, &state_root)?;

        Ok(Self {
            id: container_id.to_string(),
            state_root,
            bundle_path,
            env: env_map,
        })
    }

    /// Check if container init process is running
    ///
    /// Returns `true` if the container is in Running state, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # fn example(container: &Container) {
    /// if container.is_running() {
    ///     println!("Container is running");
    /// }
    /// # }
    /// ```
    pub fn is_running(&self) -> bool {
        let container_state_path = self.container_state_path();
        match start::load_container_status(&container_state_path) {
            Ok(status) => {
                use libcontainer::container::ContainerStatus;
                matches!(status, ContainerStatus::Running)
            }
            Err(_) => false,
        }
    }

    /// Get container ID
    ///
    /// Returns the unique container identifier.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # fn example(container: &Container) {
    /// println!("Container ID: {}", container.id());
    /// # }
    /// ```
    #[allow(dead_code)] // API completeness, may be used by future RPC handlers
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Create a command builder for executing processes in this container
    ///
    /// Returns a Command builder. Use `.cmd()` to set the program to execute.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # async fn example(container: &Container) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut child = container
    ///     .exec()
    ///     .cmd("ls")
    ///     .args(&["-la", "/tmp"])
    ///     .env("FOO", "bar")
    ///     .spawn()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn cmd(&self) -> ContainerCommand {
        ContainerCommand::new(self.id.clone(), self.state_root.clone(), self.env.clone())
    }

    /// Diagnose why container is not running
    ///
    /// Provides detailed information for debugging container startup failures.
    /// Gathers container state, process information, and common failure indicators.
    ///
    /// # Returns
    ///
    /// A diagnostic message with container ID, status, PID, and process state.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use guest::container::Container;
    /// # fn example(container: &Container) {
    /// if !container.is_running() {
    ///     let diagnostics = container.diagnose_exit();
    ///     eprintln!("Container failed: {}", diagnostics);
    /// }
    /// # }
    /// ```
    pub fn diagnose_exit(&self) -> String {
        let container_state_path = self.container_state_path();

        // Try to load container state from libcontainer
        match LibContainer::load(container_state_path.clone()) {
            Ok(libcontainer) => {
                let status = libcontainer.status();
                let pid = libcontainer.pid();

                let mut diagnostics = vec![
                    format!("Container ID: {}", self.id),
                    format!("Status: {:?}", status),
                ];

                if let Some(pid) = pid {
                    diagnostics.push(format!("PID: {}", pid));

                    // Try to get process state information
                    #[cfg(target_os = "linux")]
                    {
                        if let Ok(proc) = procfs::process::Process::new(pid.as_raw()) {
                            if let Ok(stat) = proc.stat() {
                                if let Ok(state) = stat.state() {
                                    diagnostics.push(format!("Process state: {:?}", state));
                                }
                            }
                        } else {
                            diagnostics.push("Process: no longer exists (exited)".to_string());
                        }
                    }
                } else {
                    diagnostics.push(
                        "PID: none (init process never started or exited immediately)".to_string(),
                    );
                }

                // Check for common issues
                if !self.bundle_path.exists() {
                    diagnostics.push(format!(
                        "Bundle path missing: {}",
                        self.bundle_path.display()
                    ));
                }

                diagnostics.join(", ")
            }
            Err(e) => {
                format!(
                    "Container ID: {}, Failed to load container state from {}: {}",
                    self.id,
                    container_state_path.display(),
                    e
                )
            }
        }
    }

    fn container_state_path(&self) -> PathBuf {
        self.state_root.join(&self.id)
    }
}

// ====================
// Cleanup
// ====================

impl Drop for Container {
    fn drop(&mut self) {
        tracing::debug!(container_id = %self.id, "Cleaning up container");

        let container_state_path = self.container_state_path();

        if let Ok(mut container) = LibContainer::load(container_state_path) {
            kill::kill_container(&mut container);
            kill::delete_container(&mut container);
        }

        start::cleanup_bundle_directory(&self.bundle_path);

        tracing::debug!(container_id = %self.id, "Container cleanup complete");
    }
}
