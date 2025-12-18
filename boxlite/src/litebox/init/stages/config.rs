//! Stage 4: Configuration construction.
//!
//! Builds InstanceSpec from prepared components.
//! Includes disk creation (minimal I/O).

use crate::litebox::init::types::{
    ConfigInput, ConfigOutput, ResolvedVolume, RootfsPrepResult, resolve_user_volumes,
};
use crate::net::{NetworkBackendConfig, NetworkBackendFactory};
use crate::rootfs::operations::fix_rootfs_permissions;
use crate::runtime::constants::{guest_paths, mount_tags};
use crate::vmm::{Entrypoint, FsShares, InstanceSpec};
use crate::volumes::{BackingFormat, BlockDeviceManager, DiskFormat, Qcow2Helper};
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
        input.init_rootfs,
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
        RootfsPrepResult::DiskImage { .. }
    ) {
        Some(disk_device_path)
    } else {
        None
    };

    // Create COW child disk for init rootfs (protects shared base from writes)
    let (init_rootfs, init_disk) =
        create_init_disk(input.layout, input.init_rootfs, &mut block_manager)?;

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
        init_rootfs,
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
    rootfs_result: &RootfsPrepResult,
    user_volumes: &[ResolvedVolume],
    container_id: &str,
) -> BoxliteResult<FsShares> {
    let mut shares = FsShares::new();

    // Single "shared" virtiofs share for container directories
    // Guest mounts this to /run/boxlite/shared/
    #[cfg(target_os = "linux")]
    shares.add(mount_tags::SHARED, layout.shared_dir(), false);
    #[cfg(target_os = "macos")]
    shares.add(mount_tags::SHARED, layout.mounts_dir().to_path_buf(), false);

    // Strategy-specific shares
    match rootfs_result {
        RootfsPrepResult::Merged(path) => {
            shares.add(mount_tags::ROOTFS, path.clone(), false);
        }
        RootfsPrepResult::Layers { layers_dir, .. } => {
            shares.add(mount_tags::LAYERS, layers_dir.clone(), true);
            let container_layout = layout.shared_layout().container(container_id);
            let container_root = container_layout.root();
            fix_rootfs_permissions(container_root)?;
            shares.add(mount_tags::SHARED, container_root.to_path_buf(), false);
        }
        RootfsPrepResult::DiskImage { .. } => {
            // Rootfs is on block device, no virtiofs needed
        }
    }

    for vol in user_volumes {
        shares.add(&vol.tag, vol.host_path.clone(), vol.read_only);
    }

    Ok(shares)
}

fn build_guest_entrypoint(
    transport: &Transport,
    ready_transport: &Transport,
    init_rootfs: &crate::runtime::initrf::InitRootfs,
    options: &crate::runtime::options::BoxOptions,
) -> BoxliteResult<Entrypoint> {
    let listen_uri = transport.to_uri();
    let ready_notify_uri = ready_transport.to_uri();

    // Start with init image's env
    let mut env: Vec<(String, String)> = init_rootfs.env.clone();

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
    rootfs_result: &RootfsPrepResult,
) -> BoxliteResult<crate::volumes::Disk> {
    let qcow2_helper = Qcow2Helper::new();
    let disk_path = layout.disk_path();

    // Check if using disk-based rootfs
    if let RootfsPrepResult::DiskImage {
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

/// Create COW child disk for init rootfs.
///
/// Protects the shared base init rootfs from writes by creating a per-box
/// qcow2 overlay. Returns the updated InitRootfs with device path and the
/// COW disk (to prevent cleanup on drop).
fn create_init_disk(
    layout: &crate::runtime::layout::BoxFilesystemLayout,
    init_rootfs: &crate::runtime::initrf::InitRootfs,
    block_manager: &mut BlockDeviceManager,
) -> BoxliteResult<(
    crate::runtime::initrf::InitRootfs,
    Option<crate::volumes::Disk>,
)> {
    let mut init_rootfs = init_rootfs.clone();

    let init_disk = if let crate::runtime::initrf::Strategy::Disk { ref disk_path, .. } =
        init_rootfs.strategy
    {
        let base_disk_path = disk_path;

        // Get base disk size
        let base_size = std::fs::metadata(base_disk_path)
            .map(|m| m.len())
            .unwrap_or(512 * 1024 * 1024);

        // Create COW child disk
        let init_disk_path = layout.root().join("init.qcow2");
        let qcow2_helper = Qcow2Helper::new();
        let init_disk = qcow2_helper.create_cow_child_disk(
            base_disk_path,
            BackingFormat::Raw,
            &init_disk_path,
            base_size,
        )?;

        // Register COW child (not the base)
        let device_path = block_manager.add_disk(init_disk.path(), DiskFormat::Qcow2);

        // Update strategy with COW child disk path and device
        init_rootfs.strategy = crate::runtime::initrf::Strategy::Disk {
            disk_path: init_disk_path,
            device_path: Some(device_path),
        };

        Some(init_disk)
    } else {
        None
    };

    Ok((init_rootfs, init_disk))
}
