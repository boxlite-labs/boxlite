//! Virtiofs mount helper.

use std::path::Path;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use nix::mount::{mount, MsFlags};

pub struct VirtiofsMount;

impl VirtiofsMount {
    /// Mount virtiofs tag to mount point.
    pub fn mount(tag: &str, mount_point: &Path, read_only: bool) -> BoxliteResult<()> {
        tracing::info!(
            "Mounting virtiofs: {} → {} ({})",
            tag,
            mount_point.display(),
            if read_only { "ro" } else { "rw" }
        );

        // Create mount point
        std::fs::create_dir_all(mount_point).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create mount point {}: {}",
                mount_point.display(),
                e
            ))
        })?;

        let mut flags = MsFlags::empty();
        if read_only {
            flags |= MsFlags::MS_RDONLY;
        }

        mount(
            Some(tag),
            mount_point,
            Some("virtiofs"),
            flags,
            None::<&str>,
        )
        .map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to mount virtiofs {} to {}: {}",
                tag,
                mount_point.display(),
                e
            ))
        })?;

        tracing::info!(
            "Mounted virtiofs: {} → {} ({})",
            tag,
            mount_point.display(),
            if read_only { "ro" } else { "rw" }
        );
        Ok(())
    }
}
