//! Stage 4: VMM configuration.
//!
//! Builds VMM InstanceSpec from prepared components.
//! Includes COW disk creation for rootfs and guest rootfs.

use crate::disk::{BackingFormat, DiskFormat, Qcow2Helper};
use crate::litebox::init::types::{
    ConfigInput, ConfigOutput, ContainerRootfsPrepResult, ResolvedVolume, resolve_user_volumes,
};
use crate::net::{NetworkBackendConfig, NetworkBackendFactory};
use crate::rootfs::operations::fix_rootfs_permissions;
use crate::runtime::constants::{guest_paths, mount_tags};
use crate::vmm::{Entrypoint, FsShares, InstanceSpec};
use crate::volumes::BlockDeviceManager;
use boxlite_shared::errors::BoxliteResult;
use boxlite_shared::{BoxliteError, Transport};
use std::collections::{HashMap, HashSet};

/// Build box configuration.
///
/// **Single Responsibility**: Assemble all config objects.
pub async fn run(input: ConfigInput<'_>) -> BoxliteResult<ConfigOutput> {
    // Transport setup
    let transport = Transport::unix(input.layout.socket_path());
    let ready_transport = Transport::unix(input.layout.ready_socket_path());

    let user_volumes = resolve_user_volumes(&input.options.volumes)?;

    // Prepare container directories (image/, rw/, rootfs/)
    // This is done here because we now know we're creating a container
    input
        .layout
        .shared_layout()
        .container(input.container_id.as_str())
        .prepare()?;

    let fs_shares = build_fs_shares(
        input.layout,
        &input.rootfs.rootfs_result,
        &user_volumes,
        input.container_id.as_str(),
    )?;

    // Guest entrypoint
    let guest_entrypoint = build_guest_entrypoint(
        &transport,
        &ready_transport,
        input.guest_rootfs,
        input.options,
    )?;

    // Network backend
    let network_backend = setup_networking(&input.rootfs.container_config, input.options)?;

    // Create disks based on rootfs strategy
    let disk = create_disks(
        input.layout,
        &input.rootfs.image,
        &input.rootfs.rootfs_result,
    )
    .await?;

    // Register block devices
    let mut block_manager = BlockDeviceManager::new();
    let disk_device_path = block_manager.add_disk(disk.path(), DiskFormat::Qcow2);

    // For DiskImage mode, the disk IS the rootfs disk
    let rootfs_device_path = if matches!(
        input.rootfs.rootfs_result,
        ContainerRootfsPrepResult::DiskImage { .. }
    ) {
        Some(disk_device_path)
    } else {
        None
    };

    // Create COW child disk for guest rootfs (protects shared base from writes)
    let (guest_rootfs, init_disk) =
        create_guest_rootfs_disk(input.layout, input.guest_rootfs, &mut block_manager)?;

    let block_devices = block_manager.build();

    // Assemble config
    let box_config = InstanceSpec {
        cpus: input.options.cpus,
        memory_mib: input.options.memory_mib,
        fs_shares,
        block_devices,
        guest_entrypoint,
        transport: transport.clone(),
        ready_transport: ready_transport.clone(),
        guest_rootfs,
        network_backend_endpoint: network_backend.as_ref().map(|b| b.endpoint()).transpose()?,
        home_dir: input.home_dir.clone(),
        console_output: None,
    };

    Ok(ConfigOutput {
        box_config,
        network_backend,
        disk,
        user_volumes,
        init_disk,
        rootfs_device_path,
    })
}

fn build_fs_shares(
    layout: &crate::runtime::layout::BoxFilesystemLayout,
    rootfs_result: &ContainerRootfsPrepResult,
    user_volumes: &[ResolvedVolume],
    container_id: &str,
) -> BoxliteResult<FsShares> {
    let mut shares = FsShares::new();

    // Shared directory virtiofs - needed by all strategies for host-guest communication
    shares.add(mount_tags::SHARED, layout.shared_dir(), false);

    // Strategy-specific shares
    if let ContainerRootfsPrepResult::Merged(path) = rootfs_result {
        shares.add(mount_tags::ROOTFS, path.clone(), false);
    } else if let ContainerRootfsPrepResult::Layers { layers_dir, .. } = rootfs_result {
        shares.add(mount_tags::LAYERS, layers_dir.clone(), true);
        let container_layout = layout.shared_layout().container(container_id);
        let container_root = container_layout.root();
        fix_rootfs_permissions(container_root)?;
        // Override SHARED with container-specific path for Layers mode
        shares.add(mount_tags::SHARED, container_root.to_path_buf(), false);
    }
    // DiskImage: rootfs on block device, only needs SHARED (already added above)

    for vol in user_volumes {
        shares.add(&vol.tag, vol.host_path.clone(), vol.read_only);
    }

    Ok(shares)
}

fn build_guest_entrypoint(
    transport: &Transport,
    ready_transport: &Transport,
    guest_rootfs: &crate::runtime::guest_rootfs::GuestRootfs,
    options: &crate::runtime::options::BoxOptions,
) -> BoxliteResult<Entrypoint> {
    let listen_uri = transport.to_uri();
    let ready_notify_uri = ready_transport.to_uri();

    // Start with guest rootfs env
    let mut env: Vec<(String, String)> = guest_rootfs.env.clone();

    // Override with user env vars
    for (key, value) in &options.env {
        env.retain(|(k, _)| k != key);
        env.push((key.clone(), value.clone()));
    }

    // Inject RUST_LOG from host
    if !env.iter().any(|(k, _)| k == "RUST_LOG")
        && let Ok(rust_log) = std::env::var("RUST_LOG")
        && !rust_log.is_empty()
    {
        env.push(("RUST_LOG".to_string(), rust_log));
    }

    Ok(Entrypoint {
        executable: format!("{}/boxlite-guest", guest_paths::BIN_DIR),
        args: vec![
            "--listen".to_string(),
            listen_uri,
            "--notify".to_string(),
            ready_notify_uri,
        ],
        env,
    })
}

fn setup_networking(
    container_config: &crate::images::ContainerConfig,
    options: &crate::runtime::options::BoxOptions,
) -> BoxliteResult<Option<Box<dyn crate::net::NetworkBackend>>> {
    let mut port_map: HashMap<u16, u16> = HashMap::new();

    // Step 1: Collect guest ports that user wants to customize
    // User-provided mappings should override image defaults for the same guest port
    let user_guest_ports: HashSet<u16> = options.ports.iter().map(|p| p.guest_port).collect();

    // Step 2: Image exposed ports (only add default 1:1 mapping if user didn't override)
    for port in container_config.tcp_ports() {
        if !user_guest_ports.contains(&port) {
            port_map.insert(port, port);
        }
    }

    // Step 3: User-provided mappings (always applied)
    for port in &options.ports {
        let host_port = port.host_port.unwrap_or(port.guest_port);
        port_map.insert(host_port, port.guest_port);
    }

    let final_mappings: Vec<(u16, u16)> = port_map.into_iter().collect();

    if !final_mappings.is_empty() {
        tracing::info!(
            "Port mappings: {} (image: {}, user: {}, overridden: {})",
            final_mappings.len(),
            container_config.exposed_ports.len(),
            options.ports.len(),
            user_guest_ports
                .intersection(&container_config.tcp_ports().into_iter().collect())
                .count()
        );
    }

    let config = NetworkBackendConfig::new(final_mappings);
    NetworkBackendFactory::create(config)
}

/// Create disks based on rootfs strategy.
///
/// Create the primary disk for the box.
///
/// In DiskImage mode, this is the rootfs disk (COW overlay of base ext4).
async fn create_disks(
    layout: &crate::runtime::layout::BoxFilesystemLayout,
    _image: &crate::images::ImageObject,
    rootfs_result: &ContainerRootfsPrepResult,
) -> BoxliteResult<crate::disk::Disk> {
    let qcow2_helper = Qcow2Helper::new();
    let disk_path = layout.disk_path();

    // Check if using disk-based rootfs
    if let ContainerRootfsPrepResult::DiskImage {
        base_disk_path,
        disk_size,
    } = rootfs_result
    {
        // Disk-based rootfs: create qcow2 COW overlay pointing to base ext4
        // This becomes /dev/vda - no separate data disk needed
        let rootfs_disk = qcow2_helper.create_cow_child_disk(
            base_disk_path,
            BackingFormat::Raw,
            &disk_path, // Use disk_path (disk.qcow2) for rootfs
            *disk_size,
        )?;
        tracing::info!(
            rootfs_disk = %rootfs_disk.path().display(),
            base_disk = %base_disk_path.display(),
            "Created rootfs COW overlay"
        );

        return Ok(rootfs_disk);
    }

    // Non-DiskImage mode not supported when USE_DISK_ROOTFS is enabled
    Err(BoxliteError::Internal(
        "Only DiskImage rootfs strategy is supported".into(),
    ))
}

/// Create COW child disk for guest rootfs.
///
/// Protects the shared base guest rootfs from writes by creating a per-box
/// qcow2 overlay. Returns the updated GuestRootfs with device path and the
/// COW disk (to prevent cleanup on drop).
fn create_guest_rootfs_disk(
    layout: &crate::runtime::layout::BoxFilesystemLayout,
    guest_rootfs: &crate::runtime::guest_rootfs::GuestRootfs,
    block_manager: &mut BlockDeviceManager,
) -> BoxliteResult<(
    crate::runtime::guest_rootfs::GuestRootfs,
    Option<crate::disk::Disk>,
)> {
    let mut guest_rootfs = guest_rootfs.clone();

    let guest_rootfs_disk =
        if let crate::runtime::guest_rootfs::Strategy::Disk { ref disk_path, .. } =
            guest_rootfs.strategy
        {
            let base_disk_path = disk_path;

            // Get base disk size
            let base_size = std::fs::metadata(base_disk_path)
                .map(|m| m.len())
                .unwrap_or(512 * 1024 * 1024);

            // Create COW child disk
            let guest_rootfs_disk_path = layout.root().join("guest-rootfs.qcow2");
            let qcow2_helper = Qcow2Helper::new();
            let disk = qcow2_helper.create_cow_child_disk(
                base_disk_path,
                BackingFormat::Raw,
                &guest_rootfs_disk_path,
                base_size,
            )?;

            // Register COW child (not the base)
            let device_path = block_manager.add_disk(disk.path(), DiskFormat::Qcow2);

            // Update strategy with COW child disk path and device
            guest_rootfs.strategy = crate::runtime::guest_rootfs::Strategy::Disk {
                disk_path: guest_rootfs_disk_path,
                device_path: Some(device_path),
            };

            Some(disk)
        } else {
            None
        };

    Ok((guest_rootfs, guest_rootfs_disk))
}
