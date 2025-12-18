//! Filesystem ownership and permission utilities.

use std::path::Path;
use std::process::Command;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use nix::libc;

/// Fixes filesystem ownership to match current process uid:gid.
pub struct OwnershipFixer;

impl OwnershipFixer {
    /// Fix ownership of all files/directories to current uid:gid if needed.
    ///
    /// Checks ownership first and skips if already correct.
    /// Uses chown -R for efficiency.
    pub fn fix_if_needed(path: &Path) -> BoxliteResult<()> {
        let current_uid = unsafe { libc::getuid() };
        let current_gid = unsafe { libc::getgid() };

        // Check if ownership fix is needed by sampling root and subdirectories
        if Self::ownership_matches(path, current_uid, current_gid) {
            tracing::debug!(
                "Ownership of {} already matches {}:{}",
                path.display(),
                current_uid,
                current_gid
            );
            return Ok(());
        }

        let owner = format!("{}:{}", current_uid, current_gid);

        tracing::info!("Fixing ownership of {} to {}", path.display(), owner);

        let start = std::time::Instant::now();

        // Use chown -R for efficiency (much faster than walking in Rust)
        let output = Command::new("chown")
            .args(["-R", &owner])
            .arg(path)
            .output()
            .map_err(|e| BoxliteError::Storage(format!("Failed to run chown: {}", e)))?;

        let duration = start.elapsed();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                "chown -R {} {} had errors (took {:?}): {}",
                owner,
                path.display(),
                duration,
                stderr
            );
        } else {
            tracing::info!(
                "Fixed ownership of {} to {} in {:?}",
                path.display(),
                owner,
                duration
            );
        }

        Ok(())
    }

    /// Check if ownership of root and subdirectories matches expected uid:gid.
    fn ownership_matches(path: &Path, expected_uid: u32, expected_gid: u32) -> bool {
        use std::os::unix::fs::MetadataExt;

        // Check root directory
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.uid() != expected_uid || meta.gid() != expected_gid {
                return false;
            }
        } else {
            return false;
        }

        // Sample a few subdirectories/files
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.take(5).flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.uid() != expected_uid || meta.gid() != expected_gid {
                        return false;
                    }
                }
            }
        }

        true
    }
}
