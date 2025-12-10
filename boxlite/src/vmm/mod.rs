//! Engine abstraction for Boxlite runtime.

use crate::portal::GuestSession;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;

/// Raw metrics collected from Box processes.
#[derive(Clone, Debug, Default)]
pub struct VmmMetrics {
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub disk_bytes: Option<u64>,
}

pub mod engine;
pub mod factory;
pub mod krun;
pub mod registry;

use crate::runtime::initrf::InitRootfs;
pub use engine::{Vmm, VmmConfig, VmmInstance};
pub use factory::VmmFactory;
pub use registry::create_engine;

/// Available sandbox engine implementations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum VmmKind {
    Libkrun,
    Firecracker,
}

impl FromStr for VmmKind {
    type Err = BoxliteError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "libkrun" => Ok(VmmKind::Libkrun),
            "firecracker" => Ok(VmmKind::Firecracker),
            _ => Err(BoxliteError::Engine(format!(
                "Unknown engine type: '{}'. Supported: libkrun, firecracker",
                s
            ))),
        }
    }
}

/// Trait implemented by engine-specific Box controllers.
#[async_trait::async_trait]
pub trait VmmController: Send {
    async fn start(&mut self, bundle: &InstanceSpec) -> BoxliteResult<GuestSession>;
    fn stop(&mut self) -> BoxliteResult<()>;
    fn metrics(&self) -> BoxliteResult<VmmMetrics>;
    fn is_running(&self) -> bool;
}

/// Volume configuration for host-to-guest file sharing
///
/// Encapsulates virtiofs mounts that expose host directories to the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    pub tag: String,
    pub host_path: PathBuf,
    pub read_only: bool,
}

/// Volume configuration for host-to-guest file sharing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mounts {
    mounts: Vec<MountConfig>,
}

impl Mounts {
    /// Create a new volume configuration
    pub fn new() -> Self {
        Self { mounts: Vec::new() }
    }

    pub fn add(&mut self, tag: impl Into<String>, path: PathBuf, read_only: bool) {
        self.mounts.push(MountConfig {
            tag: tag.into(),
            host_path: path,
            read_only,
        });
    }

    // Get all mounts
    pub fn mounts(&self) -> &[MountConfig] {
        &self.mounts
    }
}

/// Disk image format.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DiskFormat {
    /// Raw disk image (no format header).
    Raw,
    /// QCOW2 (QEMU Copy-On-Write v2).
    Qcow2,
}

impl DiskFormat {
    /// Convert to string for FFI.
    pub fn as_str(&self) -> &'static str {
        match self {
            DiskFormat::Raw => "raw",
            DiskFormat::Qcow2 => "qcow2",
        }
    }
}

/// Configuration for a single disk attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskConfig {
    /// Block device ID (e.g., "vda", "vdb").
    /// Guest will see this as /dev/{block_id}
    pub block_id: String,

    /// Path to disk image file on host.
    pub disk_path: PathBuf,

    /// Whether to mount read-only.
    pub read_only: bool,

    /// Disk image format ("raw", "qcow2", etc.).
    pub format: DiskFormat,
}

/// Configuration for attaching disk images via virtio-blk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Disks {
    disks: Vec<DiskConfig>,
}

impl Disks {
    /// Create a new disk attachments configuration.
    pub fn new() -> Self {
        Self { disks: Vec::new() }
    }

    /// Add a disk to attach.
    pub fn add(&mut self, disk: DiskConfig) {
        self.disks.push(disk);
    }

    /// Get all disk configurations.
    pub fn disks(&self) -> &[DiskConfig] {
        &self.disks
    }
}

/// Complete configuration for a Box instance.
///
/// BoxConfig contains volume mounts, guest agent entrypoint,
/// communication channel, and additional environment variables.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct InstanceSpec {
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    /// Volume mounts from host to guest
    pub volumes: Mounts,
    /// Disk attachments via virtio-blk
    pub disks: Disks,
    /// Guest agent entrypoint (e.g., /boxlite/bin/boxlite-guest)
    pub guest_entrypoint: Entrypoint,
    /// Host-side transport for gRPC communication
    pub transport: boxlite_shared::Transport,
    /// Host-side transport for ready notification (host listens, guest connects when ready)
    pub ready_transport: boxlite_shared::Transport,
    /// Resolved rootfs path and assembly strategy
    pub init_rootfs: InitRootfs,
    /// Network connection info (serializable, passed to subprocess)
    /// Contains the socket path or connection method to use
    pub network_backend_endpoint: Option<crate::net::NetworkBackendEndpoint>,
    /// Home directory for boxlite runtime (~/.boxlite or BOXLITE_HOME)
    pub home_dir: PathBuf,
    /// Optional file path to redirect console output (kernel/init messages)
    pub console_output: Option<PathBuf>,
}

/// Entrypoint configuration that the guest should run.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entrypoint {
    pub executable: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_format_as_str() {
        assert_eq!(DiskFormat::Raw.as_str(), "raw");
        assert_eq!(DiskFormat::Qcow2.as_str(), "qcow2");
    }

    #[test]
    fn test_disk_config_creation() {
        let disk = DiskConfig {
            block_id: "vda".to_string(),
            disk_path: PathBuf::from("/tmp/test.qcow2"),
            read_only: false,
            format: DiskFormat::Qcow2,
        };

        assert_eq!(disk.block_id, "vda");
        assert_eq!(disk.disk_path, PathBuf::from("/tmp/test.qcow2"));
        assert!(!disk.read_only);
        assert_eq!(disk.format, DiskFormat::Qcow2);
    }

    #[test]
    fn test_disk_attachments() {
        let mut disks = Disks::new();
        assert_eq!(disks.disks().len(), 0);

        disks.add(DiskConfig {
            block_id: "vda".to_string(),
            disk_path: PathBuf::from("/tmp/test.qcow2"),
            read_only: false,
            format: DiskFormat::Qcow2,
        });
        assert_eq!(disks.disks().len(), 1);

        disks.add(DiskConfig {
            block_id: "vdb".to_string(),
            disk_path: PathBuf::from("/tmp/scratch.raw"),
            read_only: true,
            format: DiskFormat::Raw,
        });
        assert_eq!(disks.disks().len(), 2);

        // Verify first disk
        assert_eq!(disks.disks()[0].block_id, "vda");
        assert_eq!(disks.disks()[0].format, DiskFormat::Qcow2);

        // Verify second disk
        assert_eq!(disks.disks()[1].block_id, "vdb");
        assert_eq!(disks.disks()[1].format, DiskFormat::Raw);
        assert!(disks.disks()[1].read_only);
    }

    #[test]
    fn test_disk_attachments_default() {
        let disks = Disks::default();
        assert_eq!(disks.disks().len(), 0);
    }

    #[test]
    fn test_disk_format_serialization() {
        // Test Raw format
        let raw = DiskFormat::Raw;
        let json = serde_json::to_string(&raw).unwrap();
        let deserialized: DiskFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DiskFormat::Raw);

        // Test Qcow2 format
        let qcow2 = DiskFormat::Qcow2;
        let json = serde_json::to_string(&qcow2).unwrap();
        let deserialized: DiskFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DiskFormat::Qcow2);
    }

    #[test]
    fn test_disk_config_serialization() {
        let disk = DiskConfig {
            block_id: "vda".to_string(),
            disk_path: PathBuf::from("/tmp/test.qcow2"),
            read_only: true,
            format: DiskFormat::Qcow2,
        };

        let json = serde_json::to_string(&disk).unwrap();
        let deserialized: DiskConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.block_id, "vda");
        assert_eq!(deserialized.disk_path, PathBuf::from("/tmp/test.qcow2"));
        assert!(deserialized.read_only);
        assert_eq!(deserialized.format, DiskFormat::Qcow2);
    }
}
