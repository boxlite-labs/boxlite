//! Unified rootfs builder for all preparation needs.

use crate::images::{ImageObject, extract_layer_tarball_streaming};
use crate::rootfs::{CopyMode, CopyMountOptions, copy_based_mount};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::{Path, PathBuf};

/// Unified builder for all rootfs preparation needs
pub struct RootfsBuilder;

impl RootfsBuilder {
    /// Create a new rootfs builder
    pub fn new() -> Self {
        Self
    }

    /// Prepare rootfs from an OCI images with copy-based mount and fallback
    ///
    /// This implementation uses a two-tier approach:
    /// 1. **Try copy-based mount** (VFS-style with layer caching)
    ///    - Extract each layer to cache once
    ///    - Stack layers using copy-based mounts
    ///    - Fast for repeated builds (cached layers)
    /// 2. **Fallback to extraction-based mount** (original approach)
    ///    - Extract all layers directly to destination
    ///    - Slower but more robust
    ///
    /// # Arguments
    /// * `dest` - Destination directory for the prepared rootfs
    /// * `images` - OCI images object containing layers
    ///
    /// # Returns
    /// * `PreparedRootfs` - Path to prepared rootfs (no cleanup responsibility)
    ///
    /// # Idempotency
    /// If `dest` already exists and contains a valid rootfs, this method skips
    /// preparation and ensures metadata consistency.
    pub async fn prepare(
        &self,
        dest: PathBuf,
        image: &ImageObject,
    ) -> BoxliteResult<PreparedRootfs> {
        tracing::info!("Preparing rootfs at {}", dest.display());

        // Try copy-based mount first (VFS-style with caching)
        let prepared = match self.prepare_copy_based(&dest, image).await {
            Ok(prepared) => {
                tracing::info!("✅ Rootfs prepared using copy-based mount");
                prepared
            }
            Err(e) => {
                tracing::warn!(
                    "Copy-based mount failed: {}, falling back to extraction-based mount",
                    e
                );

                // Clean up partially created destination directory
                if dest.exists() {
                    tracing::debug!(
                        "Cleaning up partially created destination: {}",
                        dest.display()
                    );
                    if let Err(cleanup_err) = std::fs::remove_dir_all(&dest) {
                        tracing::warn!(
                            "Failed to clean up destination during fallback: {}",
                            cleanup_err
                        );
                        // Continue anyway - extraction-based mount will overwrite
                    }
                }

                // Fallback to extraction-based mount (original approach)
                self.prepare_extraction_based(&dest, image).await?
            }
        };

        // Configure DNS (create /etc/resolv.conf) - done ONCE after rootfs is ready
        super::configure_container_dns(&dest)?;

        Ok(prepared)
    }

    /// Prepare rootfs using VFS-style copy-based mount with layer caching
    ///
    /// This is the preferred method as it caches extracted layers for reuse.
    async fn prepare_copy_based(
        &self,
        dest: &Path,
        image: &ImageObject,
    ) -> BoxliteResult<PreparedRootfs> {
        tracing::info!("Attempting copy-based mount with layer caching");

        // Get extracted layer directories (with caching)
        let extracted_layers = image.layer_extracted().await?;

        if extracted_layers.is_empty() {
            return Err(BoxliteError::Storage(
                "Cannot prepare rootfs with no layers".into(),
            ));
        }

        tracing::info!(
            "Stacking {} cached layers directly to destination",
            extracted_layers.len()
        );

        // Stack layers directly to destination
        // IMPORTANT: Whiteouts are processed INLINE during copy (not as separate phase)
        // When copying a layer, .wh.* files delete corresponding files from destination
        for (idx, layer_dir) in extracted_layers.iter().enumerate() {
            if idx == 0 {
                // First layer: copy to dest
                tracing::debug!(
                    "Copying base layer {}/{}: {} -> {}",
                    idx + 1,
                    extracted_layers.len(),
                    layer_dir.display(),
                    dest.display()
                );

                let mount = copy_based_mount(
                    layer_dir,
                    dest,
                    CopyMountOptions {
                        copy_xattrs: true,
                        copy_mode: CopyMode::Content,
                        ignore_chown_errors: false,
                    },
                )?;

                // Unmount (no-op)
                mount.unmount()?;
            } else {
                // Subsequent layers: copy on top, processing whiteouts inline
                tracing::debug!(
                    "Overlaying layer {}/{}: {} (whiteouts processed inline)",
                    idx + 1,
                    extracted_layers.len(),
                    layer_dir.display()
                );

                // Copy this layer on top, whiteouts handled during copy
                copy_directory_overlay(layer_dir, dest)?;
            }
        }

        // Fix rootfs permissions for container compatibility
        // crate::util::fix_rootfs_permissions(dest)?;

        tracing::info!("✅ Rootfs prepared at {}", dest.display());
        Ok(PreparedRootfs {
            path: dest.to_path_buf(),
        })
    }

    /// Prepare rootfs using extraction-based mount (original approach)
    ///
    /// This is the fallback method that extracts all layers directly to destination.
    async fn prepare_extraction_based(
        &self,
        dest: &PathBuf,
        image: &ImageObject,
    ) -> BoxliteResult<PreparedRootfs> {
        tracing::info!("Using extraction-based mount (fallback)");

        std::fs::create_dir_all(dest).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create rootfs directory {}: {}",
                dest.display(),
                e
            ))
        })?;

        let layer_tarballs = image.layer_tarballs().await;
        if layer_tarballs.is_empty() {
            return Err(BoxliteError::Storage(
                "Cannot prepare rootfs with no layers".into(),
            ));
        }

        // Extract layers sequentially directly to dest
        for (idx, tarball) in layer_tarballs.iter().enumerate() {
            tracing::debug!(
                "Extracting layer {}/{}: {}",
                idx + 1,
                layer_tarballs.len(),
                tarball.display()
            );
            extract_layer_tarball_streaming(tarball, dest)?;
        }

        // Fix permissions
        crate::rootfs::operations::fix_rootfs_permissions(dest)?;

        tracing::info!("✅ Rootfs prepared using extraction-based mount");
        Ok(PreparedRootfs { path: dest.clone() })
    }
}

impl Default for RootfsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Simple data holder for prepared rootfs path (no cleanup responsibility)
pub struct PreparedRootfs {
    pub path: PathBuf,
}

/// Circular symlink info for deferred handling
struct LoopSymlink {
    rel_path: PathBuf,
    target: PathBuf,
}

/// Check if a symlink is circular (points to itself)
fn is_symlink_loop(path: &Path) -> bool {
    let link_name = match path.file_name() {
        Some(n) => n.to_string_lossy(),
        None => return false,
    };

    let target = match std::fs::read_link(path) {
        Ok(t) => t,
        Err(_) => return false,
    };

    let target_str = target.to_string_lossy();

    // Check 1: Exact self-reference (thunar -> thunar)
    if target_str == link_name {
        return true;
    }

    // Check 2: Case-insensitive match for macOS (Thunar -> thunar)
    #[cfg(target_os = "macos")]
    if target_str.eq_ignore_ascii_case(&link_name) {
        return true;
    }

    false
}

/// Execute cp command with metadata preservation and CoW support
///
/// Platform-specific behavior:
/// - macOS: Tries `cp -ac` (clonefile) first, falls back to `cp -a` on cross-device errors
/// - Linux: Uses `cp -a --reflink=auto` (auto-fallback built-in)
fn execute_copy_with_metadata(src: &Path, dst: &Path) -> BoxliteResult<()> {
    use std::process::Command;

    std::fs::create_dir_all(dst).map_err(|e| {
        BoxliteError::Storage(format!("Failed to create dst dir {}: {}", dst.display(), e))
    })?;

    #[cfg(target_os = "macos")]
    {
        // Try with clonefile first
        let output = Command::new("cp")
            .args(["-ac", "--"])
            .arg(format!("{}/.", src.display()))
            .arg(dst)
            .output()
            .map_err(|e| BoxliteError::Storage(format!("Failed to execute cp: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);

            // If clonefile fails due to cross-device link, retry with regular copy
            if stderr.contains("clonefile failed") && stderr.contains("Cross-device link") {
                tracing::debug!(
                    "clonefile failed with cross-device error, retrying with regular copy"
                );

                let output_retry = Command::new("cp")
                    .args(["-a", "--"])
                    .arg(format!("{}/.", src.display()))
                    .arg(dst)
                    .output()
                    .map_err(|e| BoxliteError::Storage(format!("Failed to execute cp: {}", e)))?;

                if !output_retry.status.success() {
                    let stderr_retry = String::from_utf8_lossy(&output_retry.stderr);
                    return Err(BoxliteError::Storage(format!(
                        "cp -a {} -> {} failed: {}",
                        src.display(),
                        dst.display(),
                        stderr_retry.trim()
                    )));
                }
            } else {
                return Err(BoxliteError::Storage(format!(
                    "cp -a {} -> {} failed: {}",
                    src.display(),
                    dst.display(),
                    stderr.trim()
                )));
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let output = Command::new("cp")
            .args(["-a", "--reflink=auto", "--"])
            .arg(format!("{}/.", src.display()))
            .arg(dst)
            .output()
            .map_err(|e| BoxliteError::Storage(format!("Failed to execute cp: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BoxliteError::Storage(format!(
                "cp -a {} -> {} failed: {}",
                src.display(),
                dst.display(),
                stderr.trim()
            )));
        }
    }

    Ok(())
}

/// Copy a directory on top of another, overlaying files and processing whiteouts
///
/// This simulates overlay filesystem behavior with OCI whiteout support:
/// - Files in src overwrite files in dst
/// - Directories are merged
/// - Metadata preserved via `cp -a`: permissions, timestamps, xattrs, ownership
/// - Whiteouts processed before copy:
///   - `.wh.filename` → delete `filename` from dst
///   - `.wh..wh..opq` → opaque directory, remove entire dst dir
/// - Circular symlinks in dst are handled specially to avoid ELOOP errors
fn copy_directory_overlay(src: &Path, dst: &Path) -> BoxliteResult<()> {
    use std::collections::HashSet;
    use std::time::Instant;
    use walkdir::WalkDir;

    let total_start = Instant::now();

    // Step 1a: Process whiteouts in src and collect marker paths
    let step1_start = Instant::now();
    let mut markers: HashSet<PathBuf> = HashSet::new();

    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|e| {
            BoxliteError::Storage(format!("Failed to walk source directory: {}", e))
        })?;

        let src_path = entry.path();
        if let Some(filename) = src_path.file_name() {
            let filename_str = filename.to_string_lossy();

            if let Some(target_name) = filename_str.strip_prefix(".wh.") {
                let rel_path = src_path
                    .strip_prefix(src)
                    .map_err(|e| BoxliteError::Storage(format!("Strip prefix: {}", e)))?;

                // Store marker for later removal
                markers.insert(dst.join(rel_path));

                if target_name == ".wh..opq" {
                    // Opaque: remove entire directory in dst
                    if let Some(parent) = src_path.parent() {
                        let rel_parent = parent.strip_prefix(src).unwrap_or(Path::new(""));
                        let dst_dir = dst.join(rel_parent);
                        if dst_dir.exists() {
                            std::fs::remove_dir_all(&dst_dir).map_err(|e| {
                                BoxliteError::Storage(format!(
                                    "Failed to remove opaque dir {}: {}",
                                    dst_dir.display(),
                                    e
                                ))
                            })?;
                            tracing::debug!("Opaque: removed {}", dst_dir.display());
                        }
                    }
                } else {
                    // Regular whiteout: delete target in dst
                    if let Some(parent) = src_path.parent() {
                        let rel_parent = parent.strip_prefix(src).unwrap_or(Path::new(""));
                        let target_path = dst.join(rel_parent).join(target_name);

                        if let Ok(meta) = std::fs::symlink_metadata(&target_path) {
                            if meta.is_dir() {
                                std::fs::remove_dir_all(&target_path).ok();
                            } else {
                                std::fs::remove_file(&target_path).ok();
                            }
                            tracing::debug!("Whiteout: removed {}", target_path.display());
                        }
                    }
                }
            }
        }
    }

    // Step 1b: Find circular symlinks in dst (these block cp -a with ELOOP)
    let mut loop_symlinks: Vec<LoopSymlink> = Vec::new();

    if dst.exists() {
        for entry in WalkDir::new(dst).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let dst_path = entry.path();
            if let Ok(meta) = std::fs::symlink_metadata(dst_path)
                && meta.is_symlink()
                && is_symlink_loop(dst_path)
                && let Ok(rel_path) = dst_path.strip_prefix(dst)
            {
                let target = std::fs::read_link(dst_path).unwrap_or_default();
                loop_symlinks.push(LoopSymlink {
                    rel_path: rel_path.to_path_buf(),
                    target,
                });
                tracing::debug!("Found circular symlink in dst: {}", dst_path.display());
            }
        }
    }

    tracing::debug!(
        "Step 1 (whiteouts + loop symlinks): {:?}, markers={}, loops={}",
        step1_start.elapsed(),
        markers.len(),
        loop_symlinks.len()
    );

    // Step 2a: Remove circular symlinks from dst (if src has replacement)
    // This prevents ELOOP errors during cp -a
    // Use symlink_metadata to detect symlinks in src (exists() returns false for circular symlinks)
    let step2a_start = Instant::now();
    for loop_sym in &loop_symlinks {
        let src_file = src.join(&loop_sym.rel_path);
        let dst_file = dst.join(&loop_sym.rel_path);

        // Check if src has something at this path (file or symlink)
        let src_has_entry = std::fs::symlink_metadata(&src_file).is_ok();
        if src_has_entry {
            if let Err(e) = std::fs::remove_file(&dst_file) {
                tracing::warn!(
                    "Failed to remove circular symlink {}: {}",
                    dst_file.display(),
                    e
                );
            } else {
                tracing::debug!(
                    "Removed circular symlink: {} (src has replacement)",
                    dst_file.display()
                );
            }
        }
    }
    if !loop_symlinks.is_empty() {
        tracing::debug!(
            "Step 2a (remove circular symlinks): {:?}",
            step2a_start.elapsed()
        );
    }

    // Step 2b: Copy with full metadata preservation using cp -a with CoW
    let step2b_start = Instant::now();
    execute_copy_with_metadata(src, dst)?;
    tracing::debug!("Step 2b (cp -a): {:?}", step2b_start.elapsed());

    // Step 2c: Recreate circular symlinks that weren't replaced by src
    let step2c_start = Instant::now();
    let mut recreated = 0;
    for loop_sym in &loop_symlinks {
        let dst_file = dst.join(&loop_sym.rel_path);

        // If dst doesn't have this file after cp -a, recreate the original symlink
        if !dst_file.exists() && std::fs::symlink_metadata(&dst_file).is_err() {
            if let Err(e) = std::os::unix::fs::symlink(&loop_sym.target, &dst_file) {
                tracing::warn!(
                    "Failed to recreate circular symlink {} -> {}: {}",
                    dst_file.display(),
                    loop_sym.target.display(),
                    e
                );
            } else {
                tracing::debug!(
                    "Recreated circular symlink: {} -> {}",
                    dst_file.display(),
                    loop_sym.target.display()
                );
                recreated += 1;
            }
        }
    }
    if recreated > 0 {
        tracing::debug!(
            "Step 2c (recreate symlinks): {:?}, count={}",
            step2c_start.elapsed(),
            recreated
        );
    }

    // Step 3: Remove whiteout markers (just cleanup, no processing)
    let step3_start = Instant::now();
    for marker in &markers {
        if marker.exists() {
            std::fs::remove_file(marker).ok();
            tracing::trace!("Removed marker: {}", marker.display());
        }
    }
    tracing::debug!(
        "Step 3 (remove markers): {:?}, total: {:?}",
        step3_start.elapsed(),
        total_start.elapsed()
    );

    Ok(())
}
