use crate::util;
use boxlite_shared::{BoxliteError, BoxliteResult};
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

use super::{Disk, DiskFormat};

/// Get the path to the mke2fs binary.
///
/// Prefers the bundled mke2fs built from e2fsprogs-sys, falls back to system mke2fs.
fn get_mke2fs_path() -> PathBuf {
    util::find_binary("mke2fs").expect("mke2fs binary not found")
}

/// Calculate the total size needed for a directory tree on ext4.
///
/// This accounts for:
/// - File content sizes (rounded up to 4KB blocks)
/// - Inode overhead (256 bytes per file/dir/symlink)
/// - Directory entry overhead
fn calculate_dir_size(dir: &Path) -> BoxliteResult<u64> {
    const BLOCK_SIZE: u64 = 4096;
    const INODE_SIZE: u64 = 256;

    let mut total_blocks = 0u64;
    let mut entry_count = 0u64;

    for entry in WalkDir::new(dir).follow_links(false) {
        let entry = entry.map_err(|e| {
            BoxliteError::Storage(format!("Failed to walk directory {}: {}", dir.display(), e))
        })?;

        entry_count += 1;

        if let Ok(metadata) = entry.metadata() {
            if metadata.is_file() {
                // Each file needs at least one block, round up
                let file_blocks = metadata.len().div_ceil(BLOCK_SIZE);
                total_blocks += file_blocks.max(1);
            } else if metadata.is_dir() {
                // Directories need at least one block
                total_blocks += 1;
            }
        }
    }

    // Calculate total:
    // - Block storage
    // - Inode storage (entry_count * INODE_SIZE, rounded to blocks)
    let content_size = total_blocks * BLOCK_SIZE;
    let inode_size = entry_count * INODE_SIZE;

    Ok(content_size + inode_size)
}

/// Calculate appropriate disk size with ext4 overhead.
fn calculate_disk_size(source: &Path) -> u64 {
    let dir_size = calculate_dir_size(source).unwrap_or(64 * 1024 * 1024);

    // ext4 needs significant overhead for:
    // - Block groups and descriptors
    // - Inode tables
    // - Journal (typically 64-128MB)
    // - Reserved blocks for root (5% default)
    // Use 2x multiplier plus 256MB base overhead for journal and metadata
    let size_with_overhead = dir_size * 2 + 256 * 1024 * 1024;

    // Minimum 1GB to handle images with many files or large binaries
    let final_size = size_with_overhead.max(1024 * 1024 * 1024);

    tracing::debug!(
        "Calculated disk size: dir_size={}MB, with_overhead={}MB, final={}MB",
        dir_size / (1024 * 1024),
        size_with_overhead / (1024 * 1024),
        final_size / (1024 * 1024)
    );

    final_size
}

/// Create an ext4 disk image from a directory using mke2fs.
///
/// This uses the `mke2fs -d` option to populate the filesystem directly
/// from a source directory, which is much simpler than using libext2fs.
///
/// Size is automatically calculated based on directory contents with
/// appropriate overhead for ext4 metadata, journal, and reserved blocks.
///
/// Returns a non-persistent Disk (will be cleaned up on drop).
pub fn create_ext4_from_dir(source: &Path, output_path: &Path) -> BoxliteResult<Disk> {
    let size_bytes = calculate_disk_size(source);

    // mke2fs expects size in 4KB blocks
    let size_blocks = size_bytes / 4096;

    let output_str = output_path.to_str().ok_or_else(|| {
        BoxliteError::Storage(format!("Invalid output path: {}", output_path.display()))
    })?;

    let source_str = source.to_str().ok_or_else(|| {
        BoxliteError::Storage(format!("Invalid source path: {}", source.display()))
    })?;

    let mke2fs = get_mke2fs_path();

    // Use mke2fs with -d to populate from directory
    // -t ext4: create ext4 filesystem
    // -d dir: populate from directory
    // -E root_owner=0:0: set root ownership (important for containers)
    let output = Command::new(&mke2fs)
        .args([
            "-t",
            "ext4",
            "-d",
            source_str,
            "-E",
            "root_owner=0:0",
            "-F", // Force, don't ask questions
            "-q", // Quiet
            output_str,
            &size_blocks.to_string(),
        ])
        .output()
        .map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to run mke2fs ({}): {}",
                mke2fs.display(),
                e
            ))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BoxliteError::Storage(format!(
            "mke2fs failed with exit code {:?}: {}",
            output.status.code(),
            stderr
        )));
    }

    Ok(Disk::new(
        output_path.to_path_buf(),
        DiskFormat::Ext4,
        false,
    ))
}
