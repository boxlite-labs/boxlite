//! Guest VM volume management.
//!
//! Manages volumes visible to the guest VM:
//! - Virtiofs shares (host directory → guest mount point)
//! - Block devices (disk image → /dev/vdX)
//!
//! Generates configuration for both VMM layer and guest mount instructions.

use std::path::{Path, PathBuf};

use crate::disk::DiskFormat;
use crate::portal::interfaces::VolumeConfig;
use crate::vmm::{BlockDevice, BlockDevices, FsShares};

/// Tracked virtiofs share entry.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct FsShareEntry {
    pub tag: String,
    pub host_path: PathBuf,
    /// Guest mount path. None = guest determines from tag.
    pub guest_path: Option<String>,
    pub read_only: bool,
    /// Optional container_id for convention-based paths.
    pub container_id: Option<String>,
}

/// Tracked block device entry.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BlockDeviceEntry {
    pub block_id: String,
    pub device_path: String,
    pub disk_path: PathBuf,
    pub format: DiskFormat,
    pub read_only: bool,
    pub guest_mount: Option<String>,
    /// If true, guest should format device before mounting
    pub need_format: bool,
    /// If true, guest should resize filesystem after mounting
    pub need_resize: bool,
}

/// VMM layer mount configuration.
#[allow(dead_code)]
pub struct VmmMountConfig {
    pub fs_shares: FsShares,
    pub block_devices: BlockDevices,
}

/// Manages guest-level volume configuration.
///
/// Tracks virtiofs shares and block devices, generates VMM config
/// and guest mount instructions.
#[allow(dead_code)]
#[derive(Clone)]
pub struct GuestVolumeManager {
    fs_shares: Vec<FsShareEntry>,
    block_devices: Vec<BlockDeviceEntry>,
    next_block_index: u8,
    next_auto_tag_index: u32,
}

#[allow(dead_code)]
impl GuestVolumeManager {
    /// Create a new guest volume manager.
    pub fn new() -> Self {
        Self {
            fs_shares: Vec::new(),
            block_devices: Vec::new(),
            next_block_index: 0,
            next_auto_tag_index: 0,
        }
    }

    /// Add a virtiofs share.
    ///
    /// `guest_path`: Where to mount in guest. `None` = guest determines from tag.
    /// `container_id`: For user volumes, enables convention-based paths.
    pub fn add_fs_share(
        &mut self,
        tag: &str,
        host_path: PathBuf,
        guest_path: Option<&str>,
        read_only: bool,
        container_id: Option<String>,
    ) {
        self.fs_shares.push(FsShareEntry {
            tag: tag.to_string(),
            host_path,
            guest_path: guest_path.map(String::from),
            read_only,
            container_id,
        });
    }

    /// Add a block device.
    ///
    /// Returns the device path in guest (e.g., "/dev/vda").
    ///
    /// # Arguments
    /// * `disk_path` - Path to disk image on host
    /// * `format` - Disk format (Ext4/Qcow2)
    /// * `read_only` - Mount read-only
    /// * `guest_mount` - Where to mount in guest (None = don't mount)
    /// * `need_format` - Guest should format device before mounting
    /// * `need_resize` - Guest should resize filesystem after mounting
    pub fn add_block_device(
        &mut self,
        disk_path: &Path,
        format: DiskFormat,
        read_only: bool,
        guest_mount: Option<&str>,
        need_format: bool,
        need_resize: bool,
    ) -> String {
        let block_id = Self::block_id_from_index(self.next_block_index);
        self.next_block_index += 1;

        let device_path = format!("/dev/{}", block_id);

        self.block_devices.push(BlockDeviceEntry {
            block_id: block_id.clone(),
            device_path: device_path.clone(),
            disk_path: disk_path.to_path_buf(),
            format,
            read_only,
            guest_mount: guest_mount.map(String::from),
            need_format,
            need_resize,
        });

        tracing::debug!(
            block_id = %block_id,
            disk = %disk_path.display(),
            read_only = %read_only,
            guest_mount = ?guest_mount,
            need_format = %need_format,
            need_resize = %need_resize,
            "Added block device"
        );

        device_path
    }

    /// Allocate next sequential auto-tag (vol0, vol1, ...).
    pub fn next_auto_tag(&mut self) -> String {
        let tag = format!("vol{}", self.next_auto_tag_index);
        self.next_auto_tag_index += 1;
        tag
    }

    /// Build VMM layer configuration.
    pub fn build_vmm_config(&self) -> VmmMountConfig {
        let mut fs_shares = FsShares::new();
        for entry in &self.fs_shares {
            fs_shares.add(&entry.tag, entry.host_path.clone(), entry.read_only);
        }

        let mut block_devices = BlockDevices::new();
        for entry in &self.block_devices {
            // Map disk format to VMM block format:
            // - Ext4 filesystem → Raw block image
            // - Qcow2 → Qcow2 (COW format)
            let vmm_format = match entry.format {
                DiskFormat::Ext4 => crate::vmm::DiskFormat::Raw,
                DiskFormat::Qcow2 => crate::vmm::DiskFormat::Qcow2,
            };
            block_devices.add(BlockDevice {
                block_id: entry.block_id.clone(),
                disk_path: entry.disk_path.clone(),
                read_only: entry.read_only,
                format: vmm_format,
            });
        }

        VmmMountConfig {
            fs_shares,
            block_devices,
        }
    }

    /// Build guest mount instructions.
    pub fn build_guest_mounts(&self) -> Vec<VolumeConfig> {
        let mut volumes = Vec::new();

        for entry in &self.fs_shares {
            // Empty mount_point = guest determines from tag
            let mount_point = entry.guest_path.as_deref().unwrap_or("");
            volumes.push(VolumeConfig::virtiofs(
                &entry.tag,
                mount_point,
                entry.read_only,
                entry.container_id.clone(),
            ));
        }

        for entry in &self.block_devices {
            if let Some(ref mount_path) = entry.guest_mount {
                volumes.push(VolumeConfig::block_device(
                    &entry.device_path,
                    mount_path,
                    boxlite_shared::Filesystem::Ext4,
                    entry.need_format,
                    entry.need_resize,
                ));
            }
        }

        volumes
    }

    /// Generate block device ID from index (0 = vda, 1 = vdb, ...).
    fn block_id_from_index(index: u8) -> String {
        assert!(index < 26, "block device index must be < 26");
        let letter = (b'a' + index) as char;
        format!("vd{}", letter)
    }
}

impl Default for GuestVolumeManager {
    fn default() -> Self {
        Self::new()
    }
}
