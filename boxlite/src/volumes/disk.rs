//! RAII-managed disk abstraction.
//!
//! Provides a disk wrapper that automatically cleans up on drop.

use std::path::{Path, PathBuf};

/// Disk image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    /// Ext4 filesystem disk image.
    Ext4,
    /// QCOW2 (QEMU Copy-On-Write v2).
    Qcow2,
}

impl DiskFormat {
    /// Get string representation of this format.
    pub fn as_str(&self) -> &'static str {
        match self {
            DiskFormat::Ext4 => "ext4",
            DiskFormat::Qcow2 => "qcow2",
        }
    }
}

/// RAII-managed disk image.
///
/// Automatically deletes the disk file when dropped (unless persistent=true).
pub struct Disk {
    path: PathBuf,
    format: DiskFormat,
    /// If true, disk will NOT be deleted on drop (used for base disks)
    persistent: bool,
}

impl Disk {
    /// Create a new Disk from path.
    ///
    /// # Arguments
    /// * `path` - Path to the disk file
    /// * `format` - Disk format (Ext4 or Qcow2)
    /// * `persistent` - If true, disk won't be deleted on drop
    pub fn new(path: PathBuf, format: DiskFormat, persistent: bool) -> Self {
        Self {
            path,
            format,
            persistent,
        }
    }

    /// Get the disk path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get the disk format.
    pub fn format(&self) -> DiskFormat {
        self.format
    }

    /// Consume and leak the disk (prevent cleanup).
    ///
    /// Use when transferring ownership elsewhere or when cleanup
    /// should be handled manually.
    #[allow(dead_code)]
    pub fn leak(self) -> PathBuf {
        let path = self.path.clone();
        std::mem::forget(self);
        path
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        // Don't cleanup persistent disks (base disks)
        if self.persistent {
            tracing::debug!(
                "Skipping cleanup for persistent disk: {}",
                self.path.display()
            );
            return;
        }

        if self.path.exists() {
            if let Err(e) = std::fs::remove_file(&self.path) {
                tracing::warn!("Failed to cleanup disk {}: {}", self.path.display(), e);
            } else {
                tracing::debug!("Cleaned up disk: {}", self.path.display());
            }
        }
    }
}
