use crate::util;
use boxlite_shared::{BoxliteError, BoxliteResult};
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

use super::constants::ext4::{
    BLOCK_SIZE, DEFAULT_DIR_SIZE_BYTES, INODE_SIZE, JOURNAL_OVERHEAD_BYTES, MIN_DISK_SIZE_BYTES,
    SIZE_MULTIPLIER_DEN, SIZE_MULTIPLIER_NUM,
};
use super::{Disk, DiskFormat};

/// Get the path to the mke2fs binary.
fn get_mke2fs_path() -> PathBuf {
    util::find_binary("mke2fs").expect("mke2fs binary not found")
}

/// Get the path to the debugfs binary.
fn get_debugfs_path() -> PathBuf {
    util::find_binary("debugfs").expect("debugfs binary not found")
}

/// Calculate the total size needed for a directory tree on ext4.
///
/// This accounts for:
/// - File content sizes (rounded up to 4KB blocks)
/// - Inode overhead (256 bytes per file/dir/symlink)
/// - Directory entry overhead
fn calculate_dir_size(dir: &Path) -> BoxliteResult<u64> {
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
    let dir_size = calculate_dir_size(source).unwrap_or(DEFAULT_DIR_SIZE_BYTES);

    // ext4 overhead:
    // - Metadata (superblock, block groups, inode tables): ~1-5%
    // - Journal: 64MB
    // - We set reserved blocks to 0% via mke2fs
    // Use 1.1x multiplier (10% overhead) plus 64MB for journal
    // Testing showed ~0.5% overhead needed, 10% provides safety margin
    let size_with_overhead =
        dir_size * SIZE_MULTIPLIER_NUM / SIZE_MULTIPLIER_DEN + JOURNAL_OVERHEAD_BYTES;

    // Minimum 256MB for small images
    let final_size = size_with_overhead.max(MIN_DISK_SIZE_BYTES);

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

    // With -b 4096, mke2fs expects size in 4KB blocks
    let size_blocks = size_bytes / 4096;

    let output_str = output_path.to_str().ok_or_else(|| {
        BoxliteError::Storage(format!("Invalid output path: {}", output_path.display()))
    })?;

    let source_str = source.to_str().ok_or_else(|| {
        BoxliteError::Storage(format!("Invalid source path: {}", source.display()))
    })?;

    let mke2fs = get_mke2fs_path();

    // Use mke2fs with -d to populate from directory
    // https://man7.org/linux/man-pages/man8/mke2fs.8.html
    // -t ext4: create ext4 filesystem
    // -d dir: populate from directory
    // -m 0: no reserved blocks (default 5% is wasted for containers)
    // -E root_owner=0:0: set root ownership (important for containers)
    let output = Command::new(&mke2fs)
        .args([
            "-t",
            "ext4",
            "-b",
            "4096", // 4KB block size (explicit)
            "-d",
            source_str,
            "-m",
            "0",
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

    // Fix ownership of all files to 0:0 using debugfs
    fix_ownership_with_debugfs(output_path, source)?;

    Ok(Disk::new(
        output_path.to_path_buf(),
        DiskFormat::Ext4,
        false,
    ))
}

/// Fix ownership of all files in ext4 image to 0:0 using debugfs.
///
/// mke2fs -E root_owner=0:0 only sets the root inode.
/// This function fixes all other files/directories.
fn fix_ownership_with_debugfs(image_path: &Path, source_dir: &Path) -> BoxliteResult<()> {
    // Skip if already running as root - mke2fs creates files with current uid/gid
    let current_uid = unsafe { libc::getuid() };
    let current_gid = unsafe { libc::getgid() };
    if current_uid == 0 && current_gid == 0 {
        tracing::debug!("Running as root, skipping debugfs ownership fix");
        return Ok(());
    }

    let start = std::time::Instant::now();

    // Collect all paths relative to source_dir
    let mut paths = Vec::new();
    for entry in WalkDir::new(source_dir).follow_links(false) {
        let entry =
            entry.map_err(|e| BoxliteError::Storage(format!("Failed to walk directory: {}", e)))?;

        // Get path relative to source_dir
        let rel_path = entry
            .path()
            .strip_prefix(source_dir)
            .unwrap_or(entry.path());

        // Skip root (already handled by root_owner=0:0)
        if rel_path.as_os_str().is_empty() {
            continue;
        }

        // Convert to absolute path in ext4 (starting with /)
        let ext4_path = format!("/{}", rel_path.display());
        paths.push(ext4_path);
    }

    if paths.is_empty() {
        tracing::debug!("No files to fix ownership for");
        return Ok(());
    }

    // Build debugfs commands to set uid=0 and gid=0 for each file
    // Using sif (set inode field) command: sif <path> <field> <value>
    let mut commands = String::new();
    for path in &paths {
        // sif sets inode field by path
        commands.push_str(&format!("sif {} uid 0\n", path));
        commands.push_str(&format!("sif {} gid 0\n", path));
    }

    let debugfs = get_debugfs_path();

    // Run debugfs with commands via stdin
    let mut child = Command::new(&debugfs)
        .args(["-w", "-f", "-"])
        .arg(image_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| BoxliteError::Storage(format!("Failed to spawn debugfs: {}", e)))?;

    // Write commands to stdin
    use std::io::Write;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(commands.as_bytes()).map_err(|e| {
            BoxliteError::Storage(format!("Failed to write to debugfs stdin: {}", e))
        })?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| BoxliteError::Storage(format!("Failed to wait for debugfs: {}", e)))?;

    let duration = start.elapsed();

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            "debugfs ownership fix had errors (took {:?}): {}",
            duration,
            stderr
        );
    } else {
        tracing::info!(
            "Fixed ownership of {} files to 0:0 in {:?}",
            paths.len(),
            duration
        );
    }

    Ok(())
}
