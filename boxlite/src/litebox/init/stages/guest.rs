//! Stage 6: Guest initialization.
//!
//! Sends init configuration to guest and starts container.

use crate::litebox::init::types::{GuestInput, GuestOutput, ResolvedVolume, RootfsPrepResult};
use crate::portal::interfaces::{
    GuestInitConfig, NetworkInitConfig, RootfsInitConfig, VolumeConfig as GuestVolumeConfig,
};
use crate::runtime::constants::{guest_paths, mount_tags};
use boxlite_shared::Filesystem;
use boxlite_shared::errors::BoxliteResult;

/// Initialize guest and start container.
///
/// **Single Responsibility**: Guest RPC calls.
pub async fn run(input: GuestInput) -> BoxliteResult<GuestOutput> {
    // Build guest init config
    let guest_init_config = build_guest_init_config(
        &input.rootfs_result,
        input.is_cow_child,
        &input.user_volumes,
    )?;

    // Step 1: Guest Init
    tracing::info!("Sending guest initialization request");
    let mut guest_interface = input.guest_session.guest().await?;
    guest_interface.init(guest_init_config).await?;
    tracing::info!("Guest initialized successfully");

    // Step 2: Container Init
    tracing::info!("Sending container configuration to guest");
    let mut container_interface = input.guest_session.container().await?;
    let container_id = container_interface
        .init(
            input.container_config,
            guest_paths::STATE_ROOT,
            guest_paths::BUNDLE_ROOT,
        )
        .await?;
    tracing::info!(container_id = %container_id, "Container initialized");

    Ok(GuestOutput {
        container_id,
        guest_session: input.guest_session,
    })
}

fn build_guest_init_config(
    rootfs_result: &RootfsPrepResult,
    is_cow_child: bool,
    user_volumes: &[ResolvedVolume],
) -> BoxliteResult<GuestInitConfig> {
    // RW volume (always present)
    let mut volumes = vec![GuestVolumeConfig::virtiofs(
        mount_tags::RW,
        guest_paths::RW_DIR,
        false,
    )];

    // Block device for writable layer
    let filesystem = if is_cow_child {
        Filesystem::Unspecified
    } else {
        Filesystem::Ext4
    };
    volumes.push(GuestVolumeConfig::block_device(
        guest_paths::DISK_DEVICE,
        guest_paths::DISK_MOUNT,
        filesystem,
    ));

    // Rootfs configuration
    let rootfs = match rootfs_result {
        RootfsPrepResult::Merged(_) => {
            volumes.push(GuestVolumeConfig::virtiofs(
                mount_tags::ROOTFS,
                guest_paths::ROOTFS,
                false,
            ));
            RootfsInitConfig::Merged {
                path: guest_paths::ROOTFS.to_string(),
            }
        }
        RootfsPrepResult::Layers { layer_names, .. } => {
            let (lower_dirs, copy_layers) = if is_cow_child {
                // COW child: layers on disk
                let dirs: Vec<String> = layer_names
                    .iter()
                    .map(|name| format!("{}/layers/{}", guest_paths::DISK_MOUNT, name))
                    .collect();
                (dirs, false)
            } else {
                // Base: layers via virtiofs
                volumes.push(GuestVolumeConfig::virtiofs(
                    mount_tags::LAYERS,
                    guest_paths::LAYERS_DIR,
                    false,
                ));
                let dirs: Vec<String> = layer_names
                    .iter()
                    .map(|name| format!("{}/{}", guest_paths::LAYERS_DIR, name))
                    .collect();
                (dirs, true)
            };

            RootfsInitConfig::Overlay {
                lower_dirs,
                upper_dir: format!("{}/upper", guest_paths::DISK_MOUNT),
                work_dir: format!("{}/work", guest_paths::DISK_MOUNT),
                merged_dir: guest_paths::MERGED_DIR.to_string(),
                copy_layers,
            }
        }
    };

    for vol in user_volumes {
        volumes.push(GuestVolumeConfig::virtiofs(
            &vol.tag,
            &vol.guest_path,
            vol.read_only,
        ));
    }

    // Network configuration
    let network = Some(NetworkInitConfig {
        interface: "eth0".to_string(),
        ip: Some("192.168.127.2/24".to_string()),
        gateway: Some("192.168.127.1".to_string()),
    });

    Ok(GuestInitConfig {
        volumes,
        rootfs,
        network,
    })
}
