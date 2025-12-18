//! Stage 1: Filesystem setup.
//!
//! Creates box directory structure and sets up the mounts/ → shared/ binding.

use crate::litebox::init::types::{FilesystemInput, FilesystemOutput};
use boxlite_shared::errors::BoxliteResult;

/// Create box directories and set up shared filesystem binding.
///
/// **Single Responsibility**: Creates box-level directories and bind mount only.
/// Container directories are created in the config stage when containers are configured.
///
/// Sets up:
/// 1. Box directory structure (sockets/, mounts/)
/// 2. Bind mount from mounts/ → shared/ (Linux only)
///
/// On macOS, we skip the bind mount because libkrun's virtiofs doesn't handle
/// symlinks properly. The host config uses mounts/ directly instead of shared/.
pub fn run(input: FilesystemInput<'_>) -> BoxliteResult<FilesystemOutput> {
    let layout = input
        .runtime
        .non_sync_state
        .layout
        .box_layout(input.box_id.as_str());

    // Create base directories
    layout.prepare()?;

    // Create bind mount: mounts/ → shared/ (Linux only)
    // On macOS, virtiofs doesn't handle symlinks well, so we skip this
    // and use mounts/ directly in the host config.
    #[cfg(target_os = "linux")]
    let bind_mount = {
        use crate::fs::{BindMountConfig, create_bind_mount};
        create_bind_mount(
            &BindMountConfig::new(layout.mounts_dir(), &layout.shared_dir()).read_only(),
        )?
    };

    Ok(FilesystemOutput {
        layout,
        #[cfg(target_os = "linux")]
        _bind_mount: bind_mount,
    })
}
