//! Task: VMM Spawn - Build config and start the boxlite-shim subprocess.
//!
//! Builds VMM InstanceSpec from prepared components, then spawns a new VM
//! subprocess and returns a handler for runtime operations.

use super::{InitCtx, log_task_error, task_start};
use crate::disk::DiskFormat;
use crate::litebox::init::types::{ConfigOutput, resolve_user_volumes};
use crate::net::NetworkBackendConfig;
use crate::pipeline::PipelineTask;
use crate::runtime::constants::{guest_paths, mount_tags};
use crate::runtime::guest_rootfs::{GuestRootfs, Strategy};
use crate::runtime::types::BoxStatus;
use crate::util::find_binary;
use crate::vmm::controller::{ShimController, VmmController, VmmHandler};
use crate::vmm::{Entrypoint, InstanceSpec, VmmKind};
use crate::volumes::{ContainerVolumeManager, GuestVolumeManager};
use async_trait::async_trait;
use boxlite_shared::Transport;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::collections::{HashMap, HashSet};

pub struct VmmSpawnTask;

#[async_trait]
impl PipelineTask<InitCtx> for VmmSpawnTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        // Gather all inputs from previous tasks
        let (options, layout, rootfs_output, guest_rootfs_output, home_dir, container_id, runtime) = {
            let mut ctx = ctx.lock().await;
            let layout = ctx
                .fs_output
                .as_ref()
                .ok_or_else(|| BoxliteError::Internal("filesystem task must run first".into()))?
                .layout
                .clone();
            let rootfs_output = ctx
                .rootfs_output
                .take()
                .ok_or_else(|| BoxliteError::Internal("rootfs task must run first".into()))?;

            // Store container_config for GuestInitTask before we consume rootfs_output
            ctx.container_config = Some(rootfs_output.container_config.clone());

            let guest_rootfs_output = ctx
                .guest_rootfs_output
                .take()
                .ok_or_else(|| BoxliteError::Internal("guest_rootfs task must run first".into()))?;
            (
                ctx.config.options.clone(),
                layout,
                rootfs_output,
                guest_rootfs_output,
                ctx.home_dir.clone(),
                ctx.container_id.clone(),
                ctx.runtime.clone(),
            )
        };

        // Build config
        let config_output = build_config(
            &options,
            &layout,
            rootfs_output,
            guest_rootfs_output,
            &home_dir,
            &container_id,
        )
        .await
        .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        // Spawn VM
        let handler = spawn_vm(&box_id, &config_output.box_config)
            .await
            .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        // Update PID and status in database
        let pid = handler.pid();
        {
            let _guard_lock = runtime.acquire_write();
            let _ = runtime.box_manager.update_pid(&box_id, Some(pid));
            let _ = runtime
                .box_manager
                .update_status(&box_id, BoxStatus::Running);
        }

        let mut ctx = ctx.lock().await;
        ctx.guard.set_handler(handler);
        ctx.config_output = Some(config_output);
        Ok(())
    }

    fn name(&self) -> &str {
        "vmm_spawn"
    }
}

/// Build VMM config from prepared rootfs outputs.
async fn build_config(
    options: &crate::runtime::options::BoxOptions,
    layout: &crate::runtime::layout::BoxFilesystemLayout,
    rootfs_output: crate::litebox::init::types::ContainerRootfsOutput,
    guest_rootfs_output: crate::litebox::init::types::GuestRootfsOutput,
    home_dir: &std::path::Path,
    container_id: &crate::runtime::types::ContainerId,
) -> BoxliteResult<ConfigOutput> {
    // Transport setup
    let transport = Transport::unix(layout.socket_path());
    let ready_transport = Transport::unix(layout.ready_socket_path());

    let user_volumes = resolve_user_volumes(&options.volumes)?;

    // Prepare container directories (image/, rw/, rootfs/)
    let container_layout = layout.shared_layout().container(container_id.as_str());
    container_layout.prepare()?;

    // Create GuestVolumeManager and configure volumes
    let mut volume_mgr = GuestVolumeManager::new();

    // SHARED virtiofs - needed by all strategies
    volume_mgr.add_fs_share(mount_tags::SHARED, layout.shared_dir(), None, false, None);

    // Add container rootfs disk from rootfs_output
    // The disk was already created by ContainerRootfsTask
    let rootfs_device =
        volume_mgr.add_block_device(rootfs_output.disk.path(), DiskFormat::Qcow2, false, None);

    // Update rootfs_init with actual device path
    let rootfs_init = crate::portal::interfaces::ContainerRootfsInitConfig::DiskImage {
        device: rootfs_device,
    };

    // Add user volumes via ContainerVolumeManager
    let mut container_mgr = ContainerVolumeManager::new(&mut volume_mgr);
    for vol in &user_volumes {
        container_mgr.add_volume(
            container_id.as_str(),
            &vol.tag,
            &vol.tag,
            vol.host_path.clone(),
            &vol.guest_path,
            vol.read_only,
        );
    }
    let container_mounts = container_mgr.build_container_mounts();

    // Add guest rootfs disk from guest_rootfs_output
    let (guest_rootfs, init_disk) = configure_guest_rootfs(
        guest_rootfs_output.guest_rootfs,
        guest_rootfs_output.disk,
        &mut volume_mgr,
    )?;

    // Build VMM config from volume manager
    let vmm_config = volume_mgr.build_vmm_config();

    // Guest entrypoint
    let guest_entrypoint =
        build_guest_entrypoint(&transport, &ready_transport, &guest_rootfs, options)?;

    // Network configuration
    let network_config = build_network_config(&rootfs_output.container_config, options);

    // Assemble VMM instance spec
    let box_config = InstanceSpec {
        cpus: options.cpus,
        memory_mib: options.memory_mib,
        fs_shares: vmm_config.fs_shares,
        block_devices: vmm_config.block_devices,
        guest_entrypoint,
        transport: transport.clone(),
        ready_transport: ready_transport.clone(),
        guest_rootfs,
        network_config,
        network_backend_endpoint: None,
        home_dir: home_dir.to_path_buf(),
        console_output: None,
    };

    Ok(ConfigOutput {
        box_config,
        disk: rootfs_output.disk,
        init_disk,
        volume_mgr,
        rootfs_init,
        container_mounts,
    })
}

/// Configure guest rootfs with device path from volume manager.
///
/// Takes the guest rootfs and disk from GuestRootfsTask output,
/// adds the disk to volume manager, and updates strategy with device path.
fn configure_guest_rootfs(
    mut guest_rootfs: GuestRootfs,
    disk: Option<crate::disk::Disk>,
    volume_mgr: &mut GuestVolumeManager,
) -> BoxliteResult<(GuestRootfs, Option<crate::disk::Disk>)> {
    if let Some(ref disk) = disk
        && let Strategy::Disk { ref disk_path, .. } = guest_rootfs.strategy
    {
        // Add disk to volume manager
        let device_path = volume_mgr.add_block_device(disk.path(), DiskFormat::Qcow2, false, None);

        // Update strategy with device path
        guest_rootfs.strategy = Strategy::Disk {
            disk_path: disk_path.clone(),
            device_path: Some(device_path),
        };
    }

    Ok((guest_rootfs, disk))
}

fn build_guest_entrypoint(
    transport: &Transport,
    ready_transport: &Transport,
    guest_rootfs: &GuestRootfs,
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

/// Build network configuration from container config and options.
fn build_network_config(
    container_config: &crate::images::ContainerConfig,
    options: &crate::runtime::options::BoxOptions,
) -> Option<NetworkBackendConfig> {
    let mut port_map: HashMap<u16, u16> = HashMap::new();

    // Step 1: Collect guest ports that user wants to customize
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

    tracing::info!(
        "Port mappings: {} (image: {}, user: {}, overridden: {})",
        final_mappings.len(),
        container_config.exposed_ports.len(),
        options.ports.len(),
        user_guest_ports
            .intersection(&container_config.tcp_ports().into_iter().collect())
            .count()
    );

    // Always return Some - gvproxy provides virtio-net (eth0) even without port mappings
    Some(NetworkBackendConfig::new(final_mappings))
}

/// Spawn VM subprocess and return handler.
async fn spawn_vm(box_id: &str, config: &InstanceSpec) -> BoxliteResult<Box<dyn VmmHandler>> {
    let mut controller = ShimController::new(
        find_binary("boxlite-shim")?,
        VmmKind::Libkrun,
        box_id.to_string(),
    )?;

    controller.start(config).await
}
