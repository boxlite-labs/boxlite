//! Qcow2 disk image management.
//!
//! Creates and manages qcow2 disk images for Box block devices.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::process::Command;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use qcow2_rs::meta::Qcow2Header;

use super::constants::qcow2::{BLOCK_SIZE, CLUSTER_BITS, DEFAULT_DISK_SIZE_GB, REFCOUNT_ORDER};
use super::{Disk, DiskFormat};

/// Parsed qcow2 header information.
#[allow(dead_code)]
#[derive(Debug)]
struct Qcow2HeaderInfo {
    #[allow(dead_code)]
    version: u32,
    size: u64,
    #[allow(dead_code)]
    cluster_bits: u32,
}

/// Helper for qcow2 disk operations.
pub struct Qcow2Helper;

impl Qcow2Helper {
    /// Create a new disk manager.
    pub fn new() -> Self {
        Self
    }

    /// Create a qcow2 disk image at the specified path (uses native Rust implementation).
    ///
    /// The disk is sparse (10GB virtual size, ~200KB actual until written).
    /// Returns a RAII-managed Disk that auto-cleans up on drop (unless persistent).
    ///
    /// # Arguments
    /// * `disk_path` - Path where the disk should be created
    /// * `persistent` - If true, disk won't be deleted on drop (used for base disks)
    #[allow(dead_code)]
    pub fn create_disk(&self, disk_path: &Path, persistent: bool) -> BoxliteResult<Disk> {
        self.create_disk_native(disk_path, persistent)
    }

    /// Create a qcow2 disk image using native Rust implementation (qcow2-rs).
    #[allow(dead_code)]
    fn create_disk_native(&self, disk_path: &Path, persistent: bool) -> BoxliteResult<Disk> {
        // Ensure parent directory exists
        if let Some(parent) = disk_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create parent directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        if disk_path.exists() {
            tracing::debug!("Disk already exists: {}", disk_path.display());
            return Ok(Disk::new(
                disk_path.to_path_buf(),
                DiskFormat::Qcow2,
                persistent,
            ));
        }

        tracing::info!(
            "Creating qcow2 disk: {} ({}GB sparse)",
            disk_path.display(),
            DEFAULT_DISK_SIZE_GB
        );

        let size_bytes = DEFAULT_DISK_SIZE_GB * 1024 * 1024 * 1024;

        // Calculate required metadata size
        let (rc_table, rc_block, _l1_table) = Qcow2Header::calculate_meta_params(
            size_bytes,
            CLUSTER_BITS,
            REFCOUNT_ORDER,
            BLOCK_SIZE,
        );
        let clusters = 1 + rc_table.1 + rc_block.1;
        let buffer_size = ((clusters as usize) << CLUSTER_BITS) + BLOCK_SIZE;

        let mut header_buf = vec![0u8; buffer_size];
        Qcow2Header::format_qcow2(
            &mut header_buf,
            size_bytes,
            CLUSTER_BITS,
            REFCOUNT_ORDER,
            BLOCK_SIZE,
        )
        .map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to format qcow2 header for disk {}: {}",
                disk_path.display(),
                e
            ))
        })?;

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(disk_path)
            .map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create disk file {}: {}",
                    disk_path.display(),
                    e
                ))
            })?;

        file.write_all(&header_buf).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to write qcow2 header to disk {}: {}",
                disk_path.display(),
                e
            ))
        })?;

        tracing::info!("Created qcow2 disk: {}", disk_path.display());
        Ok(Disk::new(
            disk_path.to_path_buf(),
            DiskFormat::Qcow2,
            persistent,
        ))
    }

    /// Create a qcow2 disk image using external qemu-img binary.
    #[allow(dead_code)]
    fn create_disk_external(&self, disk_path: &Path, persistent: bool) -> BoxliteResult<Disk> {
        // Ensure parent directory exists
        if let Some(parent) = disk_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create parent directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        if disk_path.exists() {
            tracing::debug!("Disk already exists: {}", disk_path.display());
            return Ok(Disk::new(
                disk_path.to_path_buf(),
                DiskFormat::Qcow2,
                persistent,
            ));
        }

        tracing::info!(
            "Creating qcow2 disk: {} ({}GB sparse)",
            disk_path.display(),
            DEFAULT_DISK_SIZE_GB
        );

        let output = Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(disk_path)
            .arg(format!("{}G", DEFAULT_DISK_SIZE_GB))
            .output()
            .map_err(|e| BoxliteError::Storage(format!("Failed to run qemu-img: {}", e)))?;

        if !output.status.success() {
            return Err(BoxliteError::Storage(format!(
                "Failed to create qcow2 disk {}: {}",
                disk_path.display(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        tracing::info!("Created qcow2 disk: {}", disk_path.display());
        Ok(Disk::new(
            disk_path.to_path_buf(),
            DiskFormat::Qcow2,
            persistent,
        ))
    }

    /// Create COW child disk from base disk.
    ///
    /// PERF: Uses native Rust implementation instead of qemu-img subprocess.
    /// This reduces COW disk creation from ~28ms (subprocess) to ~1ms (native).
    ///
    /// This creates a qcow2 disk that uses the base disk as a backing file.
    /// Reads come from the base (shared), writes go to the child (per-box).
    ///
    /// # Arguments
    /// * `base_disk` - Path to base disk (read-only, shared)
    /// * `backing_format` - Format of the backing file (Raw or Qcow2)
    /// * `child_path` - Path where the child disk should be created
    /// * `virtual_size` - Virtual size of the disk in bytes
    ///
    /// # Returns
    /// RAII-managed Disk (auto-cleanup on drop)
    pub fn create_cow_child_disk(
        &self,
        base_disk: &Path,
        backing_format: BackingFormat,
        child_path: &Path,
        virtual_size: u64,
    ) -> BoxliteResult<Disk> {
        // Ensure parent directory exists
        if let Some(parent) = child_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create parent directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        if child_path.exists() {
            tracing::debug!("Child disk already exists: {}", child_path.display());
            return Ok(Disk::new(
                child_path.to_path_buf(),
                DiskFormat::Qcow2,
                false,
            ));
        }

        tracing::info!(
            "Creating COW child disk: {} (backing: {}, format: {})",
            child_path.display(),
            base_disk.display(),
            backing_format.as_str()
        );

        // Create COW child with backing file reference
        Self::write_cow_child_header(child_path, base_disk, backing_format, virtual_size)?;

        tracing::info!("Created COW child disk: {}", child_path.display());
        // COW children are per-box, should be cleaned up
        Ok(Disk::new(
            child_path.to_path_buf(),
            DiskFormat::Qcow2,
            false,
        ))
    }

    /// Get the virtual size of a qcow2 disk image.
    #[allow(dead_code)]
    pub fn qcow2_virtual_size(path: &Path) -> BoxliteResult<u64> {
        let header = Self::read_qcow2_header(path)?;
        Ok(header.size)
    }

    /// Read qcow2 header from disk file.
    #[allow(dead_code)]
    fn read_qcow2_header(path: &Path) -> BoxliteResult<Qcow2HeaderInfo> {
        use std::io::Read;

        let mut file = std::fs::File::open(path).map_err(|e| {
            BoxliteError::Storage(format!("Failed to open {}: {}", path.display(), e))
        })?;

        let mut header = [0u8; 104]; // qcow2 header is 104 bytes (v3)
        file.read_exact(&mut header).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to read header from {}: {}",
                path.display(),
                e
            ))
        })?;

        // Parse qcow2 header (big-endian)
        let magic = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        if magic != 0x514649fb {
            // "QFI\xfb"
            return Err(BoxliteError::Storage(format!(
                "Invalid qcow2 magic in {}: 0x{:08x}",
                path.display(),
                magic
            )));
        }

        let version = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let size = u64::from_be_bytes([
            header[24], header[25], header[26], header[27], header[28], header[29], header[30],
            header[31],
        ]);
        let cluster_bits = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);

        Ok(Qcow2HeaderInfo {
            version,
            size,
            cluster_bits,
        })
    }

    /// Write a qcow2 v3 header with backing file reference.
    ///
    /// Creates a qcow2 file that uses another file as backing store for COW.
    /// The child starts empty - all reads go to backing file.
    fn write_cow_child_header(
        child_path: &Path,
        backing_path: &Path,
        backing_format: BackingFormat,
        virtual_size: u64,
    ) -> BoxliteResult<()> {
        use std::io::Write;

        // Get absolute path for backing file
        let backing_str = backing_path
            .canonicalize()
            .map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to canonicalize backing path {}: {}",
                    backing_path.display(),
                    e
                ))
            })?
            .to_string_lossy()
            .to_string();

        let backing_bytes = backing_str.as_bytes();
        let backing_len = backing_bytes.len() as u32;

        // Backing format string for extension header
        let format_str = backing_format.as_str();
        let format_bytes = format_str.as_bytes();
        let format_len = format_bytes.len() as u32;

        // qcow2 v3 header layout:
        // 0-3:   magic (QFI\xfb)
        // 4-7:   version (3)
        // 8-15:  backing_file_offset
        // 16-19: backing_file_size
        // 20-23: cluster_bits (16 = 64KB clusters)
        // 24-31: size (virtual disk size)
        // 32-35: crypt_method (0 = none)
        // 36-39: l1_size
        // 40-47: l1_table_offset
        // 48-55: refcount_table_offset
        // 56-59: refcount_table_clusters
        // 60-63: nb_snapshots
        // 64-71: snapshots_offset
        // 72-79: incompatible_features
        // 80-87: compatible_features
        // 88-95: autoclear_features
        // 96-99: refcount_order (4 = 16-bit)
        // 100-103: header_length

        let cluster_bits: u32 = CLUSTER_BITS as u32;
        let cluster_size: u64 = 1u64 << cluster_bits;

        // Backing file goes right after the header (at offset 512)
        let backing_offset: u64 = 512;

        // L1 table calculation
        let l1_entries = virtual_size.div_ceil(cluster_size) as u32;
        let l1_size = l1_entries;
        let l1_offset = cluster_size;

        // Refcount table in cluster 2
        let refcount_offset = cluster_size * 2;
        let refcount_clusters = 1u32;

        // Header extension starts at offset 104
        let header_extension_offset = 104usize;

        // Allocate buffer for header + L1 + refcount table + refcount block
        let mut header = vec![0u8; cluster_size as usize * 4];

        // Write qcow2 v3 header
        // Magic (QFI\xfb)
        header[0..4].copy_from_slice(&0x514649fbu32.to_be_bytes());
        // Version 3
        header[4..8].copy_from_slice(&3u32.to_be_bytes());
        // Backing file offset
        header[8..16].copy_from_slice(&backing_offset.to_be_bytes());
        // Backing file size
        header[16..20].copy_from_slice(&backing_len.to_be_bytes());
        // Cluster bits
        header[20..24].copy_from_slice(&cluster_bits.to_be_bytes());
        // Virtual size
        header[24..32].copy_from_slice(&virtual_size.to_be_bytes());
        // Crypt method (0 = none)
        header[32..36].copy_from_slice(&0u32.to_be_bytes());
        // L1 size
        header[36..40].copy_from_slice(&l1_size.to_be_bytes());
        // L1 table offset
        header[40..48].copy_from_slice(&l1_offset.to_be_bytes());
        // Refcount table offset
        header[48..56].copy_from_slice(&refcount_offset.to_be_bytes());
        // Refcount table clusters
        header[56..60].copy_from_slice(&refcount_clusters.to_be_bytes());
        // Snapshots (0)
        header[60..64].copy_from_slice(&0u32.to_be_bytes());
        // Snapshots offset (0)
        header[64..72].copy_from_slice(&0u64.to_be_bytes());
        // Incompatible features (0)
        header[72..80].copy_from_slice(&0u64.to_be_bytes());
        // Compatible features (0)
        header[80..88].copy_from_slice(&0u64.to_be_bytes());
        // Autoclear features (0)
        header[88..96].copy_from_slice(&0u64.to_be_bytes());
        // Refcount order (4 = 16-bit refcounts)
        header[96..100].copy_from_slice(&(REFCOUNT_ORDER as u32).to_be_bytes());
        // Header length (104 for v3)
        header[100..104].copy_from_slice(&104u32.to_be_bytes());

        // Write backing format extension (type 0xE2792ACA)
        // This tells QEMU/libkrun the backing file format
        // Extension type: backing format (0xE2792ACA)
        header[header_extension_offset..header_extension_offset + 4]
            .copy_from_slice(&0xE2792ACAu32.to_be_bytes());
        // Extension length
        header[header_extension_offset + 4..header_extension_offset + 8]
            .copy_from_slice(&format_len.to_be_bytes());
        // Extension data (format string, padded to 8-byte boundary)
        header[header_extension_offset + 8..header_extension_offset + 8 + format_bytes.len()]
            .copy_from_slice(format_bytes);

        // End of extensions marker (type 0)
        let end_ext_offset = header_extension_offset + 8 + ((format_len as usize + 7) & !7);
        header[end_ext_offset..end_ext_offset + 4].copy_from_slice(&0u32.to_be_bytes());
        header[end_ext_offset + 4..end_ext_offset + 8].copy_from_slice(&0u32.to_be_bytes());

        // Write backing file path at offset 512
        header[backing_offset as usize..backing_offset as usize + backing_bytes.len()]
            .copy_from_slice(backing_bytes);

        // L1 table at cluster 1 - all zeros means all reads go to backing file
        // (already zero-initialized)

        // Refcount table at cluster 2 - need to mark used clusters
        let refcount_block_offset = cluster_size * 3;

        // Refcount table entry points to refcount block
        let rt_offset = refcount_offset as usize;
        header[rt_offset..rt_offset + 8].copy_from_slice(&refcount_block_offset.to_be_bytes());

        // Refcount block: mark clusters 0-3 as used (refcount = 1)
        let rb_offset = refcount_block_offset as usize;
        for i in 0..4 {
            // 16-bit refcounts (refcount_order = 4 means 2^4 = 16 bits)
            header[rb_offset + i * 2..rb_offset + i * 2 + 2].copy_from_slice(&1u16.to_be_bytes());
        }

        // Write to file
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(child_path)
            .map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create child disk {}: {}",
                    child_path.display(),
                    e
                ))
            })?;

        file.write_all(&header).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to write COW child header to {}: {}",
                child_path.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Create COW child disk using external qemu-img binary.
    #[allow(dead_code)]
    fn create_cow_child_disk_external(
        &self,
        base_disk: &Path,
        child_path: &Path,
    ) -> BoxliteResult<Disk> {
        // Ensure parent directory exists
        if let Some(parent) = child_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create parent directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        if child_path.exists() {
            tracing::debug!("Child disk already exists: {}", child_path.display());
            return Ok(Disk::new(
                child_path.to_path_buf(),
                DiskFormat::Qcow2,
                false,
            ));
        }

        tracing::info!(
            "Creating COW child disk: {} (backing: {})",
            child_path.display(),
            base_disk.display()
        );

        // Use qemu-img to create child with backing file
        // Equivalent to: qemu-img create -f qcow2 -b base.qcow2 -F qcow2 child.qcow2
        let output = Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg("-b")
            .arg(base_disk)
            .arg("-F")
            .arg("qcow2")
            .arg(child_path)
            .output()
            .map_err(|e| {
                BoxliteError::Storage(format!("Failed to run qemu-img (is it installed?): {}", e))
            })?;

        if !output.status.success() {
            return Err(BoxliteError::Storage(format!(
                "Failed to create COW child disk {}: {}",
                child_path.display(),
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        tracing::info!("Created COW child disk: {}", child_path.display());
        // COW children are per-box, should be cleaned up
        Ok(Disk::new(
            child_path.to_path_buf(),
            DiskFormat::Qcow2,
            false,
        ))
    }
}

/// Backing file format for qcow2 COW overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackingFormat {
    /// Raw disk image (ext4, etc.)
    Raw,
    /// Qcow2 disk image.
    #[allow(dead_code)]
    Qcow2,
}

impl BackingFormat {
    /// Get format string for qcow2 backing format extension.
    pub fn as_str(&self) -> &'static str {
        match self {
            BackingFormat::Raw => "raw",
            BackingFormat::Qcow2 => "qcow2",
        }
    }
}
