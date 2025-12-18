//! Linux bind mount implementation using mount(2) syscall.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

use crate::fs::BindMountConfig;

/// Linux bind mount handle.
pub struct LinuxBindMount {
    target: PathBuf,
    mounted: bool,
}

impl LinuxBindMount {
    /// Create a bind mount.
    ///
    /// Creates a bind mount from source to target with:
    /// - MS_BIND: Create a bind mount
    /// - MS_SLAVE: Prevent mount propagation (one-way from master)
    /// - MS_RDONLY (optional): Make mount read-only
    pub fn create(config: &BindMountConfig) -> BoxliteResult<Self> {
        let source = config.source;
        let target = config.target;

        // Ensure target directory exists
        std::fs::create_dir_all(target).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create bind mount target {}: {}",
                target.display(),
                e
            ))
        })?;

        // Initial bind mount
        let flags = MsFlags::MS_BIND;
        mount(Some(source), target, None::<&str>, flags, None::<&str>).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create bind mount {} -> {}: {}",
                source.display(),
                target.display(),
                e
            ))
        })?;

        debug!(
            source = %source.display(),
            target = %target.display(),
            "Created bind mount"
        );

        // Make slave to prevent propagation
        mount(
            None::<&str>,
            target,
            None::<&str>,
            MsFlags::MS_SLAVE,
            None::<&str>,
        )
        .map_err(|e| {
            // Try to unmount on error
            let _ = umount2(target, MntFlags::MNT_DETACH);
            BoxliteError::Storage(format!(
                "Failed to set slave propagation on {}: {}",
                target.display(),
                e
            ))
        })?;

        // Optionally make read-only
        if config.read_only {
            mount(
                None::<&str>,
                target,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
                None::<&str>,
            )
            .map_err(|e| {
                // Try to unmount on error
                let _ = umount2(target, MntFlags::MNT_DETACH);
                BoxliteError::Storage(format!(
                    "Failed to remount {} as read-only: {}",
                    target.display(),
                    e
                ))
            })?;

            debug!(target = %target.display(), "Remounted as read-only");
        }

        Ok(Self {
            target: target.to_path_buf(),
            mounted: true,
        })
    }

    /// Get the target path.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Cleanup the bind mount.
    pub fn cleanup(mut self) -> BoxliteResult<()> {
        self.do_cleanup()
    }

    fn do_cleanup(&mut self) -> BoxliteResult<()> {
        if !self.mounted {
            return Ok(());
        }

        self.mounted = false;

        umount2(&self.target, MntFlags::MNT_DETACH).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to unmount {}: {}",
                self.target.display(),
                e
            ))
        })?;

        debug!(target = %self.target.display(), "Unmounted bind mount");
        Ok(())
    }
}

impl Drop for LinuxBindMount {
    fn drop(&mut self) {
        if !self.mounted {
            return;
        }
        if let Err(e) = self.do_cleanup() {
            warn!(
                target = %self.target.display(),
                error = %e,
                "Failed to cleanup bind mount on drop"
            );
        }
    }
}
