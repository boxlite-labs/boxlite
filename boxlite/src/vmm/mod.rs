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

/// A filesystem share from host to guest.
///
/// Represents a virtiofs share that exposes a host directory to the guest.
/// The guest mounts this using the tag as identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsShare {
    /// Virtiofs tag (guest uses this to identify the share)
    pub tag: String,
    /// Host directory to share
    pub host_path: PathBuf,
    /// Whether the share is read-only
    pub read_only: bool,
}

/// Collection of filesystem shares from host to guest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsShares {
    shares: Vec<FsShare>,
}

impl FsShares {
    pub fn new() -> Self {
        Self { shares: Vec::new() }
    }

    pub fn add(&mut self, tag: impl Into<String>, path: PathBuf, read_only: bool) {
        self.shares.push(FsShare {
            tag: tag.into(),
            host_path: path,
            read_only,
        });
    }

    pub fn shares(&self) -> &[FsShare] {
        &self.shares
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

/// A block device attachment from host to guest.
///
/// Represents a disk image attached via virtio-blk.
/// Guest sees this as /dev/{block_id}.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockDevice {
    /// Block device ID (e.g., "vda", "vdb").
    pub block_id: String,
    /// Path to disk image file on host.
    pub disk_path: PathBuf,
    /// Whether to attach read-only.
    pub read_only: bool,
    /// Disk image format.
    pub format: DiskFormat,
}

/// Collection of block device attachments from host to guest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockDevices {
    devices: Vec<BlockDevice>,
}

impl BlockDevices {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    pub fn add(&mut self, device: BlockDevice) {
        self.devices.push(device);
    }

    pub fn devices(&self) -> &[BlockDevice] {
        &self.devices
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
    /// Filesystem shares from host to guest
    pub fs_shares: FsShares,
    /// Block device attachments via virtio-blk
    pub block_devices: BlockDevices,
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
    fn test_block_device_creation() {
        let device = BlockDevice {
            block_id: "vda".to_string(),
            disk_path: PathBuf::from("/tmp/test.qcow2"),
            read_only: false,
            format: DiskFormat::Qcow2,
        };

        assert_eq!(device.block_id, "vda");
        assert_eq!(device.disk_path, PathBuf::from("/tmp/test.qcow2"));
        assert!(!device.read_only);
        assert_eq!(device.format, DiskFormat::Qcow2);
    }

    #[test]
    fn test_block_devices() {
        let mut devices = BlockDevices::new();
        assert_eq!(devices.devices().len(), 0);

        devices.add(BlockDevice {
            block_id: "vda".to_string(),
            disk_path: PathBuf::from("/tmp/test.qcow2"),
            read_only: false,
            format: DiskFormat::Qcow2,
        });
        assert_eq!(devices.devices().len(), 1);

        devices.add(BlockDevice {
            block_id: "vdb".to_string(),
            disk_path: PathBuf::from("/tmp/scratch.raw"),
            read_only: true,
            format: DiskFormat::Raw,
        });
        assert_eq!(devices.devices().len(), 2);

        assert_eq!(devices.devices()[0].block_id, "vda");
        assert_eq!(devices.devices()[0].format, DiskFormat::Qcow2);

        assert_eq!(devices.devices()[1].block_id, "vdb");
        assert_eq!(devices.devices()[1].format, DiskFormat::Raw);
        assert!(devices.devices()[1].read_only);
    }

    #[test]
    fn test_block_devices_default() {
        let devices = BlockDevices::default();
        assert_eq!(devices.devices().len(), 0);
    }

    #[test]
    fn test_disk_format_serialization() {
        let raw = DiskFormat::Raw;
        let json = serde_json::to_string(&raw).unwrap();
        let deserialized: DiskFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DiskFormat::Raw);

        let qcow2 = DiskFormat::Qcow2;
        let json = serde_json::to_string(&qcow2).unwrap();
        let deserialized: DiskFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, DiskFormat::Qcow2);
    }

    #[test]
    fn test_block_device_serialization() {
        let device = BlockDevice {
            block_id: "vda".to_string(),
            disk_path: PathBuf::from("/tmp/test.qcow2"),
            read_only: true,
            format: DiskFormat::Qcow2,
        };

        let json = serde_json::to_string(&device).unwrap();
        let deserialized: BlockDevice = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.block_id, "vda");
        assert_eq!(deserialized.disk_path, PathBuf::from("/tmp/test.qcow2"));
        assert!(deserialized.read_only);
        assert_eq!(deserialized.format, DiskFormat::Qcow2);
    }
}
