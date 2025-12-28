//! Configuration for Boxlite.

use crate::runtime::constants::dirs as const_dirs;
use crate::runtime::constants::envs as const_envs;
use boxlite_shared::errors::BoxliteResult;
use dirs::home_dir;
use std::path::PathBuf;
/// Configuration options for BoxliteRuntime.
///
/// Users can create it with defaults and modify fields as needed.
#[derive(Clone, Debug)]
pub struct BoxliteOptions {
    pub home_dir: PathBuf,
}

impl Default for BoxliteOptions {
    fn default() -> Self {
        let home_dir = std::env::var(const_envs::BOXLITE_HOME)
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let mut path = home_dir().unwrap_or_else(|| PathBuf::from("."));
                path.push(const_dirs::BOXLITE_DIR);
                path
            });

        Self { home_dir }
    }
}

/// Options used when constructing a box.
///
/// These are user-provided options at creation time.
/// The image/rootfs is specified separately via `ContainerRuntimeConfig`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BoxOptions {
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    pub working_dir: Option<String>,
    pub env: Vec<(String, String)>,
    pub volumes: Vec<VolumeSpec>,
    pub network: NetworkSpec,
    pub ports: Vec<PortSpec>,
    /// Enable bind mount isolation for the shared mounts directory.
    ///
    /// When true, creates a read-only bind mount from `mounts/` to `shared/`,
    /// preventing the guest from modifying host-prepared files.
    ///
    /// Requires CAP_SYS_ADMIN (privileged) or FUSE (rootless) on Linux.
    /// Defaults to false.
    #[serde(default)]
    pub isolate_mounts: bool,

    /// Automatically remove box when stopped.
    ///
    /// When true (default), the box is removed from the database and its
    /// files are deleted when `stop()` is called. This is similar to
    /// Docker's `--rm` flag.
    ///
    /// When false, the box is preserved after stop and can be restarted
    /// with `runtime.get(box_id)`.
    #[serde(default = "default_auto_remove")]
    pub auto_remove: bool,
}

fn default_auto_remove() -> bool {
    false
}

impl Default for BoxOptions {
    fn default() -> Self {
        Self {
            cpus: None,
            memory_mib: None,
            working_dir: None,
            env: Vec::new(),
            volumes: Vec::new(),
            network: NetworkSpec::default(),
            ports: Vec::new(),
            isolate_mounts: false,
            auto_remove: default_auto_remove(),
        }
    }
}

impl BoxOptions {
    /// Sanitize and validate options.
    pub fn sanitize(&self) -> BoxliteResult<()> {
        #[cfg(not(target_os = "linux"))]
        if self.isolate_mounts {
            return Err(boxlite_shared::errors::BoxliteError::Unsupported(
                "isolate_mounts is only supported on Linux".to_string(),
            ));
        }
        Ok(())
    }
}

/// How to populate the box root filesystem.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RootfsSpec {
    /// Pull/resolve this registry image reference.
    Image(String),
    /// Use an already prepared rootfs at the given host path.
    RootfsPath(String),
}

impl Default for RootfsSpec {
    fn default() -> Self {
        Self::Image("alpine:latest".into())
    }
}

/// Filesystem mount specification.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct VolumeSpec {
    pub host_path: String,
    pub guest_path: String,
    pub read_only: bool,
}

/// Network isolation options.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum NetworkSpec {
    #[default]
    Isolated,
    // Host,
    // Custom(String),
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub enum PortProtocol {
    #[default]
    Tcp,
    Udp,
    // Sctp,
}

fn default_protocol() -> PortProtocol {
    PortProtocol::Tcp
}

/// Port mapping specification (host -> guest).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PortSpec {
    pub host_port: Option<u16>, // None/0 => dynamically assigned
    pub guest_port: u16,
    #[serde(default = "default_protocol")]
    pub protocol: PortProtocol,
    pub host_ip: Option<String>, // Optional bind IP, defaults to 0.0.0.0/:: if None
}
