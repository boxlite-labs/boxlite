use std::path::PathBuf;

use boxlite::runtime::constants::images;
use boxlite::runtime::options::{
    BoxOptions, BoxliteOptions, NetworkSpec, PortProtocol, PortSpec, RootfsSpec, VolumeSpec,
};
use napi_derive::napi;

/// Runtime configuration options.
///
/// Controls where BoxLite stores runtime data (images, boxes, databases).
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsOptions {
    /// Home directory for BoxLite data (defaults to ~/.boxlite)
    pub home_dir: Option<String>,
}

impl From<JsOptions> for BoxliteOptions {
    fn from(js_opts: JsOptions) -> Self {
        let mut config = BoxliteOptions::default();

        if let Some(home_dir) = js_opts.home_dir {
            config.home_dir = PathBuf::from(home_dir);
        }

        config
    }
}

/// Box creation options.
///
/// Specifies container image, resource limits, environment, volumes, and networking.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsBoxOptions {
    /// OCI image reference (e.g., "python:slim", "ghcr.io/owner/image:tag")
    pub image: Option<String>,

    /// Path to pre-prepared rootfs directory (alternative to image)
    pub rootfs_path: Option<String>,

    /// Number of CPU cores (default: 1)
    pub cpus: Option<u8>,

    /// Memory limit in MiB (default: 512)
    pub memory_mib: Option<u32>,

    /// Disk size in GB for container rootfs (sparse, grows as needed)
    pub disk_size_gb: Option<f64>,

    /// Working directory inside container (default: /root)
    pub working_dir: Option<String>,

    /// Environment variables as array of {key, value} objects
    pub env: Option<Vec<JsEnvVar>>,

    /// Volume mounts as array of volume specs
    pub volumes: Option<Vec<JsVolumeSpec>>,

    /// Network mode ("isolated" - only option currently)
    pub network: Option<String>,

    /// Port mappings as array of port specs
    pub ports: Option<Vec<JsPortSpec>>,

    /// Automatically remove box when stopped (default: false)
    pub auto_remove: Option<bool>,

    /// Run box in detached mode (survives parent process exit, default: false)
    pub detach: Option<bool>,
}

/// Environment variable specification.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsEnvVar {
    pub key: String,
    pub value: String,
}

/// Volume mount specification.
///
/// Maps a host directory to a guest path inside the container.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsVolumeSpec {
    /// Path on host machine
    pub host_path: String,

    /// Path inside container
    pub guest_path: String,

    /// Mount as read-only (default: false)
    pub read_only: Option<bool>,
}

impl From<JsVolumeSpec> for VolumeSpec {
    fn from(v: JsVolumeSpec) -> Self {
        VolumeSpec {
            host_path: v.host_path,
            guest_path: v.guest_path,
            read_only: v.read_only.unwrap_or(false),
        }
    }
}

/// Port mapping specification.
///
/// Maps a host port to a container port for network access.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsPortSpec {
    /// Port on host (None = auto-assign)
    pub host_port: Option<u16>,

    /// Port inside container
    pub guest_port: u16,

    /// Protocol ("tcp" or "udp", default: "tcp")
    pub protocol: Option<String>,

    /// Bind IP address (default: 0.0.0.0)
    pub host_ip: Option<String>,
}

impl From<JsPortSpec> for PortSpec {
    fn from(p: JsPortSpec) -> Self {
        let protocol = match p.protocol.as_deref() {
            Some("udp") => PortProtocol::Udp,
            _ => PortProtocol::Tcp,
        };

        PortSpec {
            host_port: p.host_port,
            guest_port: p.guest_port,
            protocol,
            host_ip: p.host_ip,
        }
    }
}

impl From<JsBoxOptions> for BoxOptions {
    fn from(js_opts: JsBoxOptions) -> Self {
        // Convert volumes
        let volumes = js_opts
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(VolumeSpec::from)
            .collect();

        // Convert network spec
        let network = match js_opts.network.as_deref() {
            Some(s) if s.eq_ignore_ascii_case("isolated") => NetworkSpec::Isolated,
            _ => NetworkSpec::Isolated,
        };

        // Convert ports
        let ports = js_opts
            .ports
            .unwrap_or_default()
            .into_iter()
            .map(PortSpec::from)
            .collect();

        // Convert image/rootfs_path to RootfsSpec
        let rootfs = match &js_opts.rootfs_path {
            Some(path) if !path.is_empty() => RootfsSpec::RootfsPath(path.clone()),
            _ => {
                let image = js_opts
                    .image
                    .clone()
                    .unwrap_or_else(|| images::DEFAULT.to_string());
                RootfsSpec::Image(image)
            }
        };

        // Convert environment variables
        let env = js_opts
            .env
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect();

        BoxOptions {
            cpus: js_opts.cpus,
            memory_mib: js_opts.memory_mib,
            disk_size_gb: js_opts.disk_size_gb.map(|v| v as u64),
            working_dir: js_opts.working_dir,
            env,
            rootfs,
            volumes,
            network,
            ports,
            isolate_mounts: false, // Not exposed in JS API yet
            auto_remove: js_opts.auto_remove.unwrap_or(false),
            detach: js_opts.detach.unwrap_or(false),
        }
    }
}
