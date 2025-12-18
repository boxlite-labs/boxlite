//! Stage 1: Filesystem setup.
//!
//! Creates box directory structure and optionally sets up the mounts/ → shared/ binding.

use crate::litebox::init::types::{FilesystemInput, FilesystemOutput};
use boxlite_shared::errors::BoxliteResult;

/// Create box directories and optionally set up shared filesystem binding.
///
/// Sets up:
/// 1. Box directory structure (sockets/, mounts/)
/// 2. Bind mount from mounts/ → shared/ (Linux only, when isolate_mounts=true)
pub fn run(input: FilesystemInput<'_>) -> BoxliteResult<FilesystemOutput> {
    let layout = input
        .runtime
        .non_sync_state
        .layout
        .box_layout(input.box_id.as_str());

    layout.prepare()?;

    #[cfg(target_os = "linux")]
    let bind_mount = if input.isolate_mounts {
        use crate::fs::{BindMountConfig, create_bind_mount};
        let mounts_dir = layout.mounts_dir();
        let mount = create_bind_mount(
            &BindMountConfig::new(&mounts_dir, &layout.shared_dir()).read_only(),
        )?;
        Some(mount)
    } else {
        None
    };

    #[cfg(not(target_os = "linux"))]
    let _ = input.isolate_mounts;

    Ok(FilesystemOutput {
        layout,
        #[cfg(target_os = "linux")]
        bind_mount,
    })
}
