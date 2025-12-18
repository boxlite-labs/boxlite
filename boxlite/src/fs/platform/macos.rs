//! macOS symlink fallback for bind mount simulation.
//!
//! macOS does not support Linux-style bind mounts. As a workaround,
//! we create symbolic links to simulate the same directory structure.
//!
//! Limitations:
//! - Symlinks cannot be made read-only (permissions follow target)
//! - No mount propagation semantics
//! - Some tools may not follow symlinks correctly

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::fs::BindMountConfig;

/// macOS symlink handle (simulates bind mount).
pub struct MacosSymlink {
    target: PathBuf,
    created: bool,
}

impl MacosSymlink {
    /// Create a symlink to simulate a bind mount.
    ///
    /// Note: The `read_only` flag in config has no effect on macOS.
    /// Symlinks inherit permissions from their target.
    pub fn create(config: &BindMountConfig) -> BoxliteResult<Self> {
        let source = config.source;
        let target = config.target;

        // Ensure parent directory exists
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create parent directory for {}: {}",
                    target.display(),
                    e
                ))
            })?;
        }

        // Remove existing symlink, file, or empty directory at target
        if target.exists() || target.is_symlink() {
            if target.is_symlink() || target.is_file() {
                std::fs::remove_file(target).map_err(|e| {
                    BoxliteError::Storage(format!(
                        "Failed to remove existing file/symlink {}: {}",
                        target.display(),
                        e
                    ))
                })?;
            } else if target.is_dir() {
                // Only remove if empty
                let is_empty = std::fs::read_dir(target)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false);
                if is_empty {
                    std::fs::remove_dir(target).map_err(|e| {
                        BoxliteError::Storage(format!(
                            "Failed to remove empty directory {}: {}",
                            target.display(),
                            e
                        ))
                    })?;
                } else {
                    return Err(BoxliteError::Storage(format!(
                        "Target {} exists and is not empty",
                        target.display()
                    )));
                }
            }
        }

        // Create symlink: target -> source
        std::os::unix::fs::symlink(source, target).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create symlink {} -> {}: {}",
                target.display(),
                source.display(),
                e
            ))
        })?;

        debug!(
            source = %source.display(),
            target = %target.display(),
            "Created symlink (macOS bind mount fallback)"
        );

        if config.read_only {
            debug!(
                target = %target.display(),
                "Note: read_only flag ignored on macOS (symlinks inherit target permissions)"
            );
        }

        Ok(Self {
            target: target.to_path_buf(),
            created: true,
        })
    }

    /// Get the target path (symlink location).
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Cleanup the symlink.
    pub fn cleanup(mut self) -> BoxliteResult<()> {
        self.do_cleanup()
    }

    fn do_cleanup(&mut self) -> BoxliteResult<()> {
        if !self.created {
            return Ok(());
        }

        self.created = false;

        if self.target.is_symlink() {
            std::fs::remove_file(&self.target).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to remove symlink {}: {}",
                    self.target.display(),
                    e
                ))
            })?;

            debug!(target = %self.target.display(), "Removed symlink");
        }

        Ok(())
    }
}

impl Drop for MacosSymlink {
    fn drop(&mut self) {
        if !self.created {
            return;
        }
        if let Err(e) = self.do_cleanup() {
            warn!(
                target = %self.target.display(),
                error = %e,
                "Failed to cleanup symlink on drop"
            );
        }
    }
}
