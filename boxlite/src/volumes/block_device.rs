//! Virtio block device management.

use std::path::Path;

use super::DiskFormat;
use crate::vmm::{BlockDevice, BlockDevices};

/// Virtio block device identifier.
///
/// Generates Linux virtio block device names (vda, vdb, ..., vdz).
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlockDeviceId(String);

impl BlockDeviceId {
    /// Create a new block device ID from an index (0 = vda, 1 = vdb, etc.).
    fn from_index(index: u8) -> Self {
        assert!(index < 26, "virtio block device index must be < 26");
        let letter = (b'a' + index) as char;
        Self(format!("vd{}", letter))
    }

    /// Get the block ID string (e.g., "vda").
    fn as_str(&self) -> &str {
        &self.0
    }

    /// Get the device path in guest (e.g., "/dev/vda").
    fn device_path(&self) -> String {
        format!("/dev/{}", self.0)
    }
}

/// Manages virtio block device allocation and configuration.
///
/// Handles automatic assignment of block device IDs (vda, vdb, ...) and
/// generates the final disk configuration for the VMM engine.
pub struct BlockDeviceManager {
    devices: Vec<BlockDevice>,
    next_index: u8,
}

impl BlockDeviceManager {
    /// Create a new block device manager.
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
            next_index: 0,
        }
    }

    /// Add a disk to the VM configuration.
    ///
    /// Returns the device path in guest (e.g., "/dev/vda").
    pub fn add_disk(&mut self, path: &Path, format: DiskFormat) -> String {
        let block_id = BlockDeviceId::from_index(self.next_index);
        self.next_index += 1;

        tracing::debug!("Added disk as {}: {}", block_id.as_str(), path.display());

        let device_path = block_id.device_path();

        // Convert volumes::DiskFormat to vmm::DiskFormat
        let vmm_format = match format {
            DiskFormat::Ext4 => crate::vmm::DiskFormat::Raw,
            DiskFormat::Qcow2 => crate::vmm::DiskFormat::Qcow2,
        };

        self.devices.push(BlockDevice {
            block_id: block_id.as_str().to_string(),
            disk_path: path.to_path_buf(),
            read_only: false,
            format: vmm_format,
        });

        device_path
    }

    /// Build the final block device configuration for the VMM engine.
    pub fn build(self) -> BlockDevices {
        let mut devices = BlockDevices::new();
        for device in self.devices {
            devices.add(device);
        }
        devices
    }
}

impl Default for BlockDeviceManager {
    fn default() -> Self {
        Self::new()
    }
}
