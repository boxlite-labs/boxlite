//! Unified volume mounting.
//!
//! Dispatches to the appropriate mount helper based on volume source type.

use std::path::Path;

use boxlite_shared::errors::BoxliteResult;
use boxlite_shared::{volume, Filesystem, Volume};

use super::block_device::BlockDeviceMount;
use super::virtiofs::VirtiofsMount;

/// Mount a single volume in guest.
///
/// Special handling for the "shared" tag:
/// - Mounts to GuestLayout::shared_dir() (/run/boxlite/shared/)
/// - Uses the shared filesystem pattern
pub fn mount_volume(vol: &Volume) -> BoxliteResult<()> {
    match &vol.source {
        Some(volume::Source::Virtiofs(virtiofs)) => {
            let mount_point = Path::new(&vol.mount_point);
            VirtiofsMount::mount(&virtiofs.tag, mount_point, virtiofs.read_only)
        }
        Some(volume::Source::BlockDevice(block)) => {
            let mount_point = Path::new(&vol.mount_point);
            let filesystem = Filesystem::try_from(block.filesystem).unwrap_or(Filesystem::Ext4);
            BlockDeviceMount::mount(Path::new(&block.device), mount_point, filesystem)
        }
        None => {
            tracing::warn!("Volume {} has no source, skipping", vol.mount_point);
            Ok(())
        }
    }
}

/// Mount all volumes.
pub fn mount_volumes(volumes: &[Volume]) -> BoxliteResult<()> {
    for vol in volumes {
        mount_volume(vol)?;
    }
    Ok(())
}
