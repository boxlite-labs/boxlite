//! Stage 4: Configuration construction.
//!
//! Builds InstanceSpec from prepared components.
//! Includes disk creation (minimal I/O).

use crate::litebox::init::types::{ConfigInput, ConfigOutput, RootfsPrepResult};
use crate::net::{NetworkBackendConfig, NetworkBackendFactory};
use crate::rootfs::operations::fix_rootfs_permissions;
use crate::runtime::constants::{guest_paths, mount_tags};
use crate::vmm::{DiskConfig, DiskFormat, Disks, Entrypoint, InstanceSpec, Mounts};
use crate::volumes::DiskManager;
use boxlite_shared::Transport;
use boxlite_shared::errors::BoxliteResult;
use std::collections::HashMap;

/// Build box configuration.
///
/// **Single Responsibility**: Assemble all config objects.
pub async fn run(input: ConfigInput<'_>) -> BoxliteResult<ConfigOutput> {
    // Transport setup
    let transport = Transport::unix(input.layout.socket_path());
    let ready_transport = Transport::unix(input.layout.ready_socket_path());

    // Volume configuration
    let volumes = build_volume_config(input.layout, &input.rootfs.rootfs_result)?;

    // Guest entrypoint
    let guest_entrypoint = build_guest_entrypoint(
        &transport,
        &ready_transport,
        input.init_rootfs,
        input.options,
    )?;

    // Network backend
    let network_backend = setup_networking(&input.rootfs.container_config, input.options)?;

    // Disk creation
    let (disk, is_cow_child) = create_disk(input.layout, &input.rootfs.image).await?;
    let disks = build_disk_attachments(&disk)?;

    // Assemble config
    let box_config = InstanceSpec {
        cpus: input.options.cpus,
        memory_mib: input.options.memory_mib,
        volumes,
        disks,
        guest_entrypoint,
        transport: transport.clone(),
        ready_transport: ready_transport.clone(),
        init_rootfs: input.init_rootfs.clone(),
        network_backend_endpoint: network_backend.as_ref().map(|b| b.endpoint()).transpose()?,
        home_dir: input.home_dir.clone(),
        // console_output: Some(input.layout.console_output_path()),
        console_output: None,
    };

    Ok(ConfigOutput {
        box_config,
        network_backend,
        disk,
        is_cow_child,
    })
}

fn build_volume_config(
    layout: &crate::runtime::layout::BoxFilesystemLayout,
    rootfs_result: &RootfsPrepResult,
) -> BoxliteResult<Mounts> {
    let rw_dir = layout.rw_dir();
    fix_rootfs_permissions(&rw_dir)?;

    let mut mounts = vec![(mount_tags::RW.to_string(), rw_dir)];

    match rootfs_result {
        RootfsPrepResult::Merged(path) => {
            mounts.push((mount_tags::ROOTFS.to_string(), path.clone()));
        }
        RootfsPrepResult::Layers { layers_dir, .. } => {
            mounts.push((mount_tags::LAYERS.to_string(), layers_dir.clone()));
        }
    }

    Ok(Mounts::new(mounts))
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

    // Exposed ports from image
    for port in container_config.tcp_ports() {
        port_map.insert(port, port);
    }

    // User-provided mappings
    for port in &options.ports {
        if let Some(host_port) = port.host_port {
            port_map.insert(host_port, port.guest_port);
        }
    }

    let final_mappings: Vec<(u16, u16)> = port_map.into_iter().collect();

    if !final_mappings.is_empty() {
        tracing::info!(
            "Port mappings: {} (image: {}, user: {})",
            final_mappings.len(),
            container_config.exposed_ports.len(),
            options.ports.len()
        );
    }

    let config = NetworkBackendConfig::new(final_mappings);
    NetworkBackendFactory::create(config)
}

async fn create_disk(
    layout: &crate::runtime::layout::BoxFilesystemLayout,
    image: &crate::images::ImageObject,
) -> BoxliteResult<(crate::volumes::Disk, bool)> {
    let disk_manager = DiskManager::new();
    let disk_path = layout.disk_path();

    if let Some(disk_image) = image.disk_image().await {
        // COW child from existing disk image
        let disk = disk_manager.create_cow_child_disk(disk_image.path(), &disk_path)?;
        tracing::info!(
            disk_path = %disk.path().display(),
            "Created COW child disk"
        );
        Ok((disk, true))
    } else {
        // New empty disk
        let disk = disk_manager.create_disk(&disk_path, false)?;
        tracing::info!(
            disk_path = %disk.path().display(),
            "Created empty disk for population"
        );
        Ok((disk, false))
    }
}

fn build_disk_attachments(disk: &crate::volumes::Disk) -> BoxliteResult<Disks> {
    let mut disks = Disks::new();
    disks.add(DiskConfig {
        block_id: "vda".to_string(),
        disk_path: disk.path().to_path_buf(),
        read_only: false,
        format: DiskFormat::Qcow2,
    });
    Ok(disks)
}
