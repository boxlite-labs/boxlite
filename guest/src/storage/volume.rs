//! Unified volume mounting.
//!
//! Dispatches to the appropriate mount helper based on volume source type.

use std::path::{Path, PathBuf};

use boxlite_shared::constants::mount_tags;
use boxlite_shared::errors::BoxliteResult;
use boxlite_shared::layout::GUEST_BASE;
use boxlite_shared::{volume, Filesystem, Volume};

use super::block_device::BlockDeviceMount;
use super::virtiofs::VirtiofsMount;

/// Resolve mount point from tag when mount_point is empty.
///
/// System volumes use well-known paths:
/// - SHARED → /run/boxlite/shared
/// - LAYERS (with container_id) → /run/boxlite/shared/containers/{container_id}/layers
/// - User volumes (with container_id) → /run/boxlite/shared/containers/{container_id}/volumes/{tag}
fn resolve_mount_point(tag: &str, mount_point: &str, container_id: &str) -> PathBuf {
    if !mount_point.is_empty() {
        return PathBuf::from(mount_point);
    }

    // Guest determines path based on tag
    match tag {
        mount_tags::SHARED => PathBuf::from(GUEST_BASE).join("shared"),
        mount_tags::LAYERS => {
            if container_id.is_empty() {
                // Legacy path (shouldn't happen in convention-based mode)
                PathBuf::from(GUEST_BASE).join("shared/container0_layers")
            } else {
                // Convention-based path
                let layout = boxlite_shared::layout::SharedGuestLayout::new(
                    PathBuf::from(GUEST_BASE).join("shared"),
                );
                layout.container(container_id).layers_dir()
            }
        }
        _ => {
            if container_id.is_empty() {
                tracing::warn!(
                    "Unknown tag '{}' with empty mount_point, using /mnt/{}",
                    tag,
                    tag
                );
                PathBuf::from("/mnt").join(tag)
            } else {
                // User volume with convention-based path
                let layout = boxlite_shared::layout::SharedGuestLayout::new(
                    PathBuf::from(GUEST_BASE).join("shared"),
                );
                layout.container(container_id).volume_dir(tag)
            }
        }
    }
}

/// Mount a single volume in guest.
///
/// Empty mount_point: guest determines path from tag and container_id.
pub fn mount_volume(vol: &Volume) -> BoxliteResult<()> {
    match &vol.source {
        Some(volume::Source::Virtiofs(virtiofs)) => {
            let mount_point =
                resolve_mount_point(&virtiofs.tag, &vol.mount_point, &vol.container_id);
            VirtiofsMount::mount(&virtiofs.tag, &mount_point, virtiofs.read_only)
        }
        Some(volume::Source::BlockDevice(block)) => {
            let mount_point = Path::new(&vol.mount_point);
            let filesystem = Filesystem::try_from(block.filesystem).unwrap_or(Filesystem::Ext4);
            BlockDeviceMount::mount(
                Path::new(&block.device),
                mount_point,
                filesystem,
                block.need_format,
                block.need_resize,
            )
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
