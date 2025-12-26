//! Low-level rootfs operations
//!
//! This module provides shared primitives for rootfs manipulation used by both
//! PreparedRootfs (new architecture) and RootfsBuilder (Alpine boot rootfs).
//!
//! All functions are platform-aware and handle cross-platform differences internally.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::fs;
use std::path::Path;

#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::Command;

/// Mount overlayfs combining multiple layers (Linux only).
///
/// Creates an overlayfs mount at the target directory with the specified lower,
/// upper, and work directories. Requires CAP_SYS_ADMIN capability.
///
/// # Arguments
/// * `lower_dirs` - Read-only layer directories (bottom to top order)
/// * `upper_dir` - Writable upper layer directory
/// * `work_dir` - Overlayfs work directory (must be on same filesystem as upper)
/// * `target_dir` - Mount point for the merged filesystem
///
/// # Returns
/// * `Ok(())` if mount succeeds
/// * `Err(BoxliteError)` if mount command fails
///
/// # Platform Support
/// * **Linux**: Uses kernel overlayfs via mount command
/// * **macOS**: Returns error (overlayfs not supported)
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub fn mount_overlayfs_from_layers(
    lower_dirs: &[PathBuf],
    upper_dir: &Path,
    work_dir: &Path,
    target_dir: &Path,
) -> BoxliteResult<()> {
    if lower_dirs.is_empty() {
        return Err(BoxliteError::Storage(
            "Cannot mount overlayfs with no lower directories".into(),
        ));
    }

    // Build lowerdir string: layer0:layer1:... (base to top)
    let lowerdir = lower_dirs
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(":");

    let mount_options = format!(
        "lowerdir={},upperdir={},workdir={}",
        lowerdir,
        upper_dir.display(),
        work_dir.display()
    );

    tracing::debug!("Mounting overlayfs with options: {}", mount_options);

    let output = Command::new("mount")
        .args([
            "-t",
            "overlay",
            "overlay",
            "-o",
            &mount_options,
            target_dir
                .to_str()
                .ok_or_else(|| BoxliteError::Storage("Invalid target path".into()))?,
        ])
        .output()
        .map_err(|e| BoxliteError::Storage(format!("Failed to execute mount command: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BoxliteError::Storage(format!(
            "Failed to mount overlayfs: {}",
            stderr
        )));
    }

    tracing::info!("Overlayfs mounted at {}", target_dir.display());
    Ok(())
}

/// Unmount an overlayfs mount point (Linux only).
///
/// # Arguments
/// * `mount_point` - Directory where overlayfs is mounted
///
/// # Returns
/// * `Ok(())` if unmount succeeds
/// * `Err(BoxliteError)` if unmount command fails
#[cfg(target_os = "linux")]
#[allow(dead_code)]
pub fn unmount_overlayfs(mount_point: &Path) -> BoxliteResult<()> {
    let output = Command::new("umount")
        .arg(mount_point)
        .output()
        .map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to execute umount for {}: {}",
                mount_point.display(),
                e
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BoxliteError::Storage(format!(
            "Failed to unmount overlayfs at {}: {}",
            mount_point.display(),
            stderr
        )));
    }

    tracing::debug!("Unmounted overlayfs at {}", mount_point.display());
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn unmount_overlayfs(_mount_point: &Path) -> BoxliteResult<()> {
    Ok(()) // No-op on non-Linux platforms
}

/// Process OCI whiteout files in a directory tree.
///
/// OCI images layers use special .wh.* files to indicate deletions:
/// - `.wh.filename` → delete "filename"
/// - `.wh..wh..opq` → opaque directory marker (delete all contents from lower layers)
///
/// This function recursively walks the directory tree, removes files marked
/// for deletion, and cleans up the whiteout marker files themselves.
///
/// # Arguments
/// * `dir` - Root directory to process
///
/// # Returns
/// * `Ok(())` on success
/// * `Err(BoxliteError)` if directory cannot be read
#[allow(dead_code)]
pub fn process_whiteouts(dir: &Path) -> BoxliteResult<()> {
    fn process_dir_recursive(dir: &Path) -> BoxliteResult<()> {
        let entries: Vec<_> = fs::read_dir(dir)
            .map_err(|e| {
                BoxliteError::Storage(format!("Failed to read directory {}: {}", dir.display(), e))
            })?
            .filter_map(|e| e.ok())
            .collect();

        for entry in entries {
            let path = entry.path();
            let filename = entry.file_name();
            let filename_str = filename.to_string_lossy();

            if let Some(target_name) = filename_str.strip_prefix(".wh.") {
                if target_name == ".wh..opq" {
                    // Opaque directory marker - tar extraction order already handled this
                    // Just remove the marker file
                    let _ = fs::remove_file(&path);
                } else {
                    // Whiteout file - delete the corresponding target
                    let target_path = dir.join(target_name);

                    if target_path.exists() {
                        if target_path.is_dir() {
                            let _ = fs::remove_dir_all(&target_path);
                        } else {
                            let _ = fs::remove_file(&target_path);
                        }
                        tracing::debug!("Processed whiteout: removed {}", target_path.display());
                    }

                    // Remove the whiteout marker file itself
                    let _ = fs::remove_file(&path);
                }
            } else if path.is_dir() {
                // Recursively process subdirectories
                process_dir_recursive(&path)?;
            }
        }

        Ok(())
    }

    process_dir_recursive(dir)
}

/// Fix rootfs permissions using xattr for permission virtualization.
/// The virtio-fs implementation respects the "user.containers.override_stat" attribute.
///
/// Sets directory permissions to 700 and applies user.containers.override_stat
/// per-file to preserve each file's actual permissions while virtualizing ownership to root (0:0).
/// This ensures setuid binaries and executables maintain their correct permission bits.
/// Ignores errors on symlinks and special files.
///
/// # Arguments
/// * `rootfs` - Path to the rootfs directory
///
/// # Returns
/// * `Ok(())` if permissions and xattr were set successfully
/// * `Err(...)` if critical operations failed
pub fn fix_rootfs_permissions(rootfs: &Path) -> BoxliteResult<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    tracing::info!(
        "Setting per-file xattr for rootfs permissions on {}",
        rootfs.display()
    );

    // Recursively set xattr for each file, preserving actual mode bits
    fn set_xattr_recursive(path: &Path, depth: usize) -> BoxliteResult<usize> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("Skipping {}: {}", path.display(), e);
                return Ok(0);
            }
        };

        let mut count = 0;

        // Skip symlinks early - they don't need xattr
        if metadata.file_type().is_symlink() {
            return Ok(0);
        }

        // Get actual mode bits (preserve setuid/setgid/sticky bits)
        let mode = metadata.permissions().mode() & 0o7777;

        // Format as 4-digit octal with leading zeros (e.g., "0:0:0755")
        let xattr_value = format!("0:0:{:04o}", mode);

        // Temporarily add write permission if needed to set xattr
        let needs_write = (mode & 0o200) == 0;
        if needs_write {
            let mut temp_perms = metadata.permissions();
            temp_perms.set_mode(mode | 0o200); // Add owner write
            let _ = fs::set_permissions(path, temp_perms);
        }

        // Set xattr (ignore errors on special files like device nodes)
        match xattr::set(
            path,
            "user.containers.override_stat",
            xattr_value.as_bytes(),
        ) {
            Ok(_) => {
                count += 1;
                if depth < 2 {
                    // Log first few levels for verification
                    tracing::debug!(
                        "Set xattr on {}: {} (mode: {:o})",
                        path.display(),
                        xattr_value,
                        mode
                    );
                }
            }
            Err(e) => {
                // Only log at trace level to avoid spam
                tracing::trace!("Failed to set xattr on {}: {}", path.display(), e);
            }
        }

        // Restore original permissions if we modified them
        if needs_write {
            let mut orig_perms = metadata.permissions();
            orig_perms.set_mode(mode);
            let _ = fs::set_permissions(path, orig_perms);
        }

        // Recurse into directories
        if metadata.is_dir()
            && let Ok(entries) = fs::read_dir(path)
        {
            for entry in entries.filter_map(|e| e.ok()) {
                count += set_xattr_recursive(&entry.path(), depth + 1)?;
            }
        }

        Ok(count)
    }

    let count = set_xattr_recursive(rootfs, 0)?;

    tracing::info!("✅ Per-file xattr set for {} files in rootfs", count);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_process_whiteouts_removes_target_file() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();

        // Create a file and its whiteout marker
        fs::write(dir.join("file.txt"), "content").unwrap();
        fs::write(dir.join(".wh.file.txt"), "").unwrap();

        process_whiteouts(dir).unwrap();

        // Both should be removed
        assert!(!dir.join("file.txt").exists());
        assert!(!dir.join(".wh.file.txt").exists());
    }

    #[test]
    fn test_process_whiteouts_removes_opaque_marker() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path();

        fs::write(dir.join(".wh..wh..opq"), "").unwrap();

        process_whiteouts(dir).unwrap();

        assert!(!dir.join(".wh..wh..opq").exists());
    }
}
