//! Block device mount helper.
//!
//! Mounts and formats block devices (e.g., /dev/vda).

use std::path::Path;
use std::process::Command;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use boxlite_shared::Filesystem;
use nix::libc;
use nix::mount::{mount, MsFlags};

/// Mounts and formats block devices.
pub struct BlockDeviceMount;

impl BlockDeviceMount {
    /// Mount block device with optional formatting and resizing.
    ///
    /// # Arguments
    /// * `device` - Block device path (e.g., "/dev/vda")
    /// * `mount_point` - Where to mount in guest
    /// * `filesystem` - Target filesystem type
    /// * `need_format` - If true, format device before mounting
    /// * `need_resize` - If true, resize filesystem after mounting to fill disk
    pub fn mount(
        device: &Path,
        mount_point: &Path,
        filesystem: Filesystem,
        need_format: bool,
        need_resize: bool,
    ) -> BoxliteResult<()> {
        let fs_name = filesystem_to_str(filesystem);

        tracing::info!(
            "Mounting block device: {} → {} (filesystem={:?}, format={}, resize={})",
            device.display(),
            mount_point.display(),
            filesystem,
            need_format,
            need_resize
        );

        // Check device exists
        if !device.exists() {
            return Err(BoxliteError::Storage(format!(
                "Block device not found: {}",
                device.display()
            )));
        }

        // Format if requested
        if need_format {
            Self::format(device, fs_name)?;
        } else {
            tracing::info!("Skipping format - using existing filesystem");
        }

        // Create mount point
        std::fs::create_dir_all(mount_point).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create mount point {}: {}",
                mount_point.display(),
                e
            ))
        })?;

        // PERF: Mount with noatime to reduce unnecessary disk writes.
        // - MS_NOATIME: Don't update file access times (saves ~10-20ms on mount)
        // - MS_NODIRATIME: Don't update directory access times
        // These flags significantly reduce I/O overhead, especially for read-heavy
        // workloads. Access time tracking is rarely needed in container contexts.
        let mount_flags = MsFlags::MS_NOATIME | MsFlags::MS_NODIRATIME;

        // Mount using nix
        mount(
            Some(device),
            mount_point,
            Some(fs_name),
            mount_flags,
            None::<&str>,
        )
        .map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to mount {} to {}: {}",
                device.display(),
                mount_point.display(),
                e
            ))
        })?;

        // Resize filesystem if requested (expands ext4 to fill available disk space)
        if need_resize {
            Self::resize_filesystem(device, filesystem)?;
        }

        // Fix ownership if needed (fallback in case debugfs didn't run on host)
        super::perms::OwnershipFixer::fix_if_needed(mount_point)?;

        tracing::info!(
            "Mounted block device: {} → {}",
            device.display(),
            mount_point.display()
        );

        // Log filesystem contents only when trace logging is enabled
        if tracing::enabled!(tracing::Level::TRACE) {
            Self::log_filesystem_contents(mount_point)?;
        }

        Ok(())
    }

    /// Log a glance view of mounted filesystem contents (2 levels deep).
    fn log_filesystem_contents(mount_point: &Path) -> BoxliteResult<()> {
        tracing::trace!("Filesystem structure at {}:", mount_point.display());
        Self::log_directory_tree(mount_point, 0, 2)?;
        Ok(())
    }

    /// Recursively log directory tree up to specified depth.
    fn log_directory_tree(
        path: &Path,
        current_depth: usize,
        max_depth: usize,
    ) -> BoxliteResult<()> {
        if current_depth > max_depth {
            return Ok(());
        }

        let indent = "  ".repeat(current_depth);

        match std::fs::read_dir(path) {
            Ok(entries) => {
                let mut items: Vec<_> = entries.filter_map(|e| e.ok()).collect();
                items.sort_by_key(|e| e.file_name());

                // Show empty directory indicator at root level
                if current_depth == 0 && items.is_empty() {
                    tracing::trace!("{}(empty)", indent);
                }

                for entry in items {
                    let name = entry.file_name();
                    let entry_path = entry.path();

                    if let Ok(metadata) = entry.metadata() {
                        if metadata.is_dir() {
                            tracing::trace!("{}{}/", indent, name.to_string_lossy());
                            // Recurse into subdirectory
                            let _ =
                                Self::log_directory_tree(&entry_path, current_depth + 1, max_depth);
                        } else {
                            tracing::trace!(
                                "{}{} ({})",
                                indent,
                                name.to_string_lossy(),
                                human_readable_size(metadata.len())
                            );
                        }
                    }
                }
            }
            Err(e) => {
                if current_depth == 0 {
                    tracing::warn!(
                        "Could not read filesystem contents at {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
        Ok(())
    }

    /// Resize ext4 filesystem to fill available disk space.
    ///
    /// This is used for COW disks where the qcow2 virtual size is larger
    /// than the base ext4 filesystem. resize2fs expands the filesystem online
    /// after mounting.
    ///
    /// # Errors
    /// Returns error if filesystem is not ext4 or if resize operation fails.
    fn resize_filesystem(device: &Path, filesystem: Filesystem) -> BoxliteResult<()> {
        // Only ext4 is supported for online resize
        if filesystem != Filesystem::Ext4 {
            return Err(BoxliteError::Storage(format!(
                "Filesystem resize only supported for ext4, got {:?}",
                filesystem
            )));
        }

        tracing::info!(
            "Resizing ext4 filesystem on {} to fill disk",
            device.display()
        );

        let output = Command::new("resize2fs")
            .arg(device)
            .output()
            .map_err(|e| BoxliteError::Storage(format!("Failed to execute resize2fs: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BoxliteError::Storage(format!(
                "resize2fs failed on {}: {}",
                device.display(),
                stderr.trim()
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        tracing::info!("Filesystem resize completed: {}", stdout.trim());
        Ok(())
    }

    /// Format device with specified filesystem.
    fn format(device: &Path, filesystem: &str) -> BoxliteResult<()> {
        // Debug: log user info and device status
        let uid = unsafe { libc::getuid() };
        let euid = unsafe { libc::geteuid() };
        tracing::info!(
            "Formatting {} with {} (uid={}, euid={})",
            device.display(),
            filesystem,
            uid,
            euid
        );

        // Debug: check device permissions
        if let Ok(metadata) = std::fs::metadata(device) {
            use std::os::unix::fs::MetadataExt;
            tracing::info!(
                "Device {} mode={:o}, uid={}, gid={}",
                device.display(),
                metadata.mode(),
                metadata.uid(),
                metadata.gid()
            );
        }

        let mkfs_cmd = format!("mkfs.{}", filesystem);
        let output = Command::new(&mkfs_cmd)
            .arg("-F") // Force, don't prompt
            .arg(device)
            .output()
            .map_err(|e| BoxliteError::Storage(format!("Failed to run {}: {}", mkfs_cmd, e)))?;

        if !output.status.success() {
            return Err(BoxliteError::Storage(format!(
                "Failed to format {} with {}: {}",
                device.display(),
                filesystem,
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        tracing::info!("Formatted {} successfully", device.display());
        Ok(())
    }
}

/// Convert Filesystem enum to string for mkfs command.
fn filesystem_to_str(fs: Filesystem) -> &'static str {
    match fs {
        Filesystem::Ext4 => "ext4",
        Filesystem::Unspecified => "ext4", // Default to ext4
    }
}

/// Convert bytes to human-readable size.
fn human_readable_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;

    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }

    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", size, UNITS[unit_idx])
    }
}
