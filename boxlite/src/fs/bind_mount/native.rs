//! Native bind mount using mount(2) syscall.
//!
//! Requires CAP_SYS_ADMIN capability.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use std::path::{Path, PathBuf};
use tracing::debug;

use super::{BindMountConfig, BindMountImpl, ensure_target_dir_exists};

pub struct NativeBindMount {
    target: PathBuf,
    mounted: bool,
}

impl NativeBindMount {
    pub fn create(config: &BindMountConfig) -> BoxliteResult<Self> {
        let source = config.source;
        let target = config.target;

        ensure_target_dir_exists(target)?;
        create_bind_mount(source, target)?;
        set_slave_propagation(target)?;

        if config.read_only {
            remount_read_only(target)?;
        }

        debug!(
            source = %source.display(),
            target = %target.display(),
            read_only = config.read_only,
            "Native bind mount created"
        );

        Ok(Self {
            target: target.to_path_buf(),
            mounted: true,
        })
    }
}

impl BindMountImpl for NativeBindMount {
    fn target(&self) -> &Path {
        &self.target
    }

    fn unmount(&mut self) -> BoxliteResult<()> {
        if !self.mounted {
            return Ok(());
        }
        self.mounted = false;

        umount2(&self.target, MntFlags::MNT_DETACH).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to unmount native bind mount {}: {}",
                self.target.display(),
                e
            ))
        })?;

        debug!(target = %self.target.display(), "Native bind mount unmounted");
        Ok(())
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn create_bind_mount(source: &Path, target: &Path) -> BoxliteResult<()> {
    mount(
        Some(source),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create bind mount {} -> {}: {}",
            source.display(),
            target.display(),
            e
        ))
    })
}

fn set_slave_propagation(target: &Path) -> BoxliteResult<()> {
    mount(
        None::<&str>,
        target,
        None::<&str>,
        MsFlags::MS_SLAVE,
        None::<&str>,
    )
    .map_err(|e| {
        // Cleanup on failure
        let _ = umount2(target, MntFlags::MNT_DETACH);
        BoxliteError::Storage(format!(
            "Failed to set slave propagation on {}: {}",
            target.display(),
            e
        ))
    })
}

fn remount_read_only(target: &Path) -> BoxliteResult<()> {
    mount(
        None::<&str>,
        target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .map_err(|e| {
        // Cleanup on failure
        let _ = umount2(target, MntFlags::MNT_DETACH);
        BoxliteError::Storage(format!(
            "Failed to remount {} as read-only: {}",
            target.display(),
            e
        ))
    })
}
