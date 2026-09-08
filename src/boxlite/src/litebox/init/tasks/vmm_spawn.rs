//! Task: VMM Spawn - Build config and start the boxlite-shim subprocess.
//!
//! Builds VMM InstanceSpec from prepared components, then spawns a new VM
//! subprocess and returns a handler for runtime operations.

use super::guest_entrypoint::GuestEntrypointBuilder;
use super::{InitCtx, log_task_error, task_start};
use crate::disk::DiskFormat;
use crate::images::ContainerImageConfig;
use crate::litebox::init::types::{InitPipelineContext, resolve_user_volumes};
use crate::net::{NetworkBackend, NetworkBackendConfig};
use crate::pipeline::PipelineTask;
use crate::rootfs::guest::{GuestRootfs, Strategy};
use crate::runtime::constants::{guest_paths, mount_tags};
use crate::runtime::id::BoxID;
use crate::runtime::layout::BoxFilesystemLayout;
use crate::runtime::options::{BoxOptions, NetworkSpec};
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::runtime::types::ContainerID;
use crate::util::find_binary;
use crate::vmm::controller::{ShimController, VmmController, VmmHandler};
use crate::vmm::{Entrypoint, InstanceSpec, PreparedKernel, VmmKind};
use crate::volumes::{
    ContainerMount, ContainerVolumeManager, GuestVolumeManager, stage_single_file,
};
use async_trait::async_trait;
use boxlite_shared::BoxTransport;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::PathBuf;

pub struct VmmSpawnTask;

struct VmmSpawnInputs {
    options: BoxOptions,
    layout: BoxFilesystemLayout,
    container_image_config: ContainerImageConfig,
    prepared_kernel: Option<PreparedKernel>,
    container_disk_path: PathBuf,
    container_id: ContainerID,
    runtime: SharedRuntimeImpl,
    reuse_rootfs: bool,
}

impl VmmSpawnInputs {
    fn from_context(ctx: &InitPipelineContext) -> BoxliteResult<Self> {
        let layout = ctx
            .layout
            .clone()
            .ok_or_else(|| BoxliteError::Internal("filesystem task must run first".into()))?;
        let container_image_config = ctx
            .container_image_config
            .clone()
            .ok_or_else(|| BoxliteError::Internal("rootfs task must run first".into()))?;
        let prepared_kernel = ctx
            .boot_assets
            .as_ref()
            .ok_or_else(|| BoxliteError::Internal("boot assets task must run first".into()))?
            .kernel
            .clone();
        let container_disk_path = ctx
            .container_disk
            .as_ref()
            .ok_or_else(|| BoxliteError::Internal("rootfs task must run first".into()))?
            .path()
            .to_path_buf();

        Ok(Self {
            options: ctx.config.options.clone(),
            layout,
            container_image_config,
            prepared_kernel,
            container_disk_path,
            container_id: ctx.config.container.id.clone(),
            runtime: ctx.runtime.clone(),
            reuse_rootfs: ctx.reuse_rootfs,
        })
    }
}

#[async_trait]
impl PipelineTask<InitCtx> for VmmSpawnTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        // Gather all inputs from previous tasks
        let inputs = {
            let ctx = ctx.lock().await;
            VmmSpawnInputs::from_context(&ctx)?
        };

        // Build config and get outputs
        let (instance_spec, volume_mgr, rootfs_init, container_mounts, network_backend) =
            build_config(&box_id, &inputs)
                .await
                .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        // Spawn VM
        let handler = spawn_vm(&box_id, &instance_spec, &inputs.options, &inputs.layout)
            .await
            .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        let mut ctx = ctx.lock().await;
        ctx.guard.set_handler(handler);
        ctx.volume_mgr = Some(volume_mgr);
        ctx.rootfs_init = Some(rootfs_init);
        ctx.container_mounts = Some(container_mounts);
        // Hand the box's one network backend to LiveState assembly (init/mod.rs).
        ctx.network_backend = network_backend;
        // Store CA cert PEM for Container.Init gRPC (passed as CACert proto field)
        ctx.ca_cert_pem = instance_spec
            .network_backend_spec
            .as_ref()
            .and_then(|s| s.ca_cert_pem.clone());
        Ok(())
    }

    fn name(&self) -> &str {
        "vmm_spawn"
    }
}

/// Build VMM config from prepared rootfs outputs.
async fn build_config(
    box_id: &BoxID,
    inputs: &VmmSpawnInputs,
) -> BoxliteResult<(
    InstanceSpec,
    GuestVolumeManager,
    crate::portal::interfaces::ContainerRootfsInitConfig,
    Vec<ContainerMount>,
    Option<Box<dyn NetworkBackend>>,
)> {
    let VmmSpawnInputs {
        options,
        layout,
        container_image_config,
        prepared_kernel,
        container_disk_path,
        container_id,
        runtime,
        reuse_rootfs,
    } = inputs;

    // BoxTransport setup
    let transport = BoxTransport::unix(layout.socket_path());
    let ready_transport = BoxTransport::unix(layout.ready_socket_path());

    let user_volumes = resolve_user_volumes(&options.volumes)?;

    // Prepare container directories (image/, rw/, rootfs/)
    let container_layout = layout.shared_layout().container(container_id.as_str());
    container_layout.prepare()?;

    // Create GuestVolumeManager and configure volumes
    let mut volume_mgr = GuestVolumeManager::new();

    // SHARED virtiofs - needed by all strategies
    volume_mgr.add_fs_share(mount_tags::SHARED, layout.shared_dir(), None, false, None);

    // Add container rootfs disk (COW overlay workflow):
    // 1. Base disk: Pre-built ext4 image with container layers merged
    // 2. COW disk: QCOW2 overlay with copy-on-write semantics
    //    - Inherits formatted ext4 from base (need_format=false)
    //    - May have larger virtual size if disk_size_gb specified
    // 3. Guest mount: Only resize on fresh start, not restart
    //    - Fresh start with custom size: resize2fs expands filesystem
    //    - Restart: filesystem already at correct size, skip resize
    let need_resize = options.disk_size_gb.is_some() && !reuse_rootfs;
    let rootfs_device = volume_mgr.add_block_device(
        container_disk_path,
        DiskFormat::Qcow2,
        false,
        None,
        false,       // need_format: COW child inherits formatted base
        need_resize, // need_resize: only on fresh start with custom disk size
    );

    // Update rootfs_init with actual device path and resize flag
    let rootfs_init = crate::portal::interfaces::ContainerRootfsInitConfig::DiskImage {
        device: rootfs_device,
        need_format: false, // COW child uses pre-formatted base
        need_resize,        // Only on fresh start with custom disk size
    };

    // Add user volumes via ContainerVolumeManager
    let mut container_mgr = ContainerVolumeManager::new(&mut volume_mgr);
    for vol in &user_volumes {
        // Single-file volume: stage the file into a dedicated dir under the box's
        // shared tree (already granted to the VMM sandbox) and share that dir, so
        // virtio-fs never exposes the file's host siblings. Directories share as-is.
        let share_dir = match &vol.subpath {
            None => vol.host_path.clone(),
            Some(file_name) => {
                let staging_dir = layout.shared_dir().join("user-volumes").join(&vol.tag);
                stage_single_file(&staging_dir, &vol.host_path, file_name, vol.read_only)?;
                staging_dir
            }
        };
        container_mgr.add_volume(
            container_id.as_str(),
            &vol.tag,
            &vol.tag,
            share_dir,
            &vol.guest_path,
            vol.read_only,
            vol.owner_uid,
            vol.owner_gid,
            vol.subpath.clone(),
        );
    }
    let container_mounts = container_mgr.build_container_mounts();

    // Get guest rootfs from runtime cache and configure with disk
    let guest_rootfs = runtime
        .guest_rootfs
        .get()
        .ok_or_else(|| BoxliteError::Internal("guest_rootfs not initialized".into()))?
        .clone();

    let guest_rootfs = configure_guest_rootfs(guest_rootfs, &mut volume_mgr)?;

    // Build VMM config from volume manager
    let vmm_config = volume_mgr.build_vmm_config();

    // Guest entrypoint
    let guest_entrypoint =
        build_guest_entrypoint(&transport, &ready_transport, &guest_rootfs, options)?;

    // The box's one network backend: it produces the wire spec now, and is
    // threaded on to LiveState (via the init ctx) for runtime control.
    warn_unpublished_exposed_ports(container_image_config, options);
    let network_backend = build_network_backend(options, layout, runtime)?;
    let network_backend_spec = network_backend.as_ref().map(|backend| backend.spec());

    // Assemble VMM instance spec
    let instance_spec = InstanceSpec {
        engine: VmmKind::Libkrun, // only engine — will be dynamic when others are added
        // Box identification and security
        box_id: box_id.to_string(),
        security: options.advanced.security.clone(),
        nested_virtualization: options.advanced.nested_virtualization,
        // VM resources
        cpus: options.cpus,
        memory_mib: options.memory_mib,
        kernel: prepared_kernel.clone(),
        // Filesystem and devices
        fs_shares: vmm_config.fs_shares,
        block_devices: vmm_config.block_devices,
        guest_entrypoint,
        transport: transport.clone(),
        ready_transport: ready_transport.clone(),
        guest_rootfs,
        network_backend_spec,
        network_backend_endpoint: None,
        disable_network: matches!(options.network, NetworkSpec::Disabled),
        home_dir: runtime.layout.home_dir().to_path_buf(),
        // Diagnostic files in box_dir (preserved on crash)
        console_output: Some(layout.console_output_path()),
        exit_file: layout.exit_file_path(),
        detach: options.detach,
    };

    Ok((
        instance_spec,
        volume_mgr,
        rootfs_init,
        container_mounts,
        network_backend,
    ))
}

/// Configure guest rootfs with device path from volume manager.
fn configure_guest_rootfs(
    mut guest_rootfs: GuestRootfs,
    volume_mgr: &mut GuestVolumeManager,
) -> BoxliteResult<GuestRootfs> {
    if let Strategy::Disk { ref disk_path, .. } = guest_rootfs.strategy {
        // The guest rootfs is read-only: attach the base ext4 directly, with no
        // per-box qcow2 COW overlay — the guest never writes its root disk.
        // The block device is opened read-only (bases/ is mounted RO by the
        // jailer), and the "ro" mount option in set_root_disk_remount makes the
        // guest root read-only too.
        let device_path = volume_mgr.add_block_device(
            disk_path,
            DiskFormat::Ext4,
            true, // read_only
            None,
            false, // need_format
            false, // need_resize
        );

        // Update strategy with device path
        guest_rootfs.strategy = Strategy::Disk {
            disk_path: disk_path.clone(),
            device_path: Some(device_path),
        };
    }

    Ok(guest_rootfs)
}

fn build_guest_entrypoint(
    transport: &BoxTransport,
    ready_transport: &BoxTransport,
    guest_rootfs: &GuestRootfs,
    options: &crate::runtime::options::BoxOptions,
) -> BoxliteResult<Entrypoint> {
    let listen_uri = transport.to_uri();
    let ready_notify_uri = ready_transport.to_uri();

    let executable = format!("{}/boxlite-guest", guest_paths::BIN_DIR);
    let mut builder = GuestEntrypointBuilder::new(executable);
    builder.with_arg("--listen");
    builder.with_arg(&listen_uri);
    builder.with_arg("--notify");
    builder.with_arg(&ready_notify_uri);

    // Debug vars first (prioritized - guaranteed space)
    if let Ok(v) = std::env::var("RUST_LOG") {
        builder.with_env("RUST_LOG", &v);
    }
    if let Ok(v) = std::env::var("RUST_BACKTRACE") {
        builder.with_env("RUST_BACKTRACE", &v);
    }

    // FILO order: image → user (later overrides earlier)
    for (key, value) in &guest_rootfs.env {
        builder.with_env(key, value);
    }
    for (key, value) in &options.env {
        builder.with_env(key, value);
    }

    // Secret placeholder env vars are injected in container_rootfs.rs (single source of truth).
    // The guest init process inherits them from the container environment.

    Ok(builder.build())
}

/// Create the box's **one** network backend by routing box-level policy through
/// the abstraction: assemble a
/// [`NetworkBackendConfig`] and hand it to the factory. `None` when networking is
/// disabled. The returned backend is used for both
/// its wire spec (`spec()`) and, threaded on to `LiveState`, runtime control — no
/// caller here names a concrete backend.
fn build_network_backend(
    options: &crate::runtime::options::BoxOptions,
    layout: &BoxFilesystemLayout,
    runtime: &SharedRuntimeImpl,
) -> BoxliteResult<Option<Box<dyn NetworkBackend>>> {
    // Disabled = no network at all.
    let allow_net = match &options.network {
        NetworkSpec::Enabled { allow_net } => allow_net.clone(),
        NetworkSpec::Disabled => return Ok(None),
    };

    let config = NetworkBackendConfig {
        socket_path: layout.net_backend_socket_path(),
        allow_net,
        secrets: options.secrets.clone(),
        ca_dir: layout.ca_dir(),
        net_bandwidth: options.net_bandwidth,
    };

    // Hand the config to the backend abstraction — the one backend for this box.
    Ok(runtime.network_factory.create(&config))
}

fn unpublished_exposed_tcp_ports(
    image_config: &ContainerImageConfig,
    options: &BoxOptions,
) -> Vec<u16> {
    if matches!(options.network, NetworkSpec::Disabled) {
        return Vec::new();
    }

    let published_guest_ports = options
        .ports
        .iter()
        .map(|mapping| mapping.guest_port)
        .collect::<std::collections::HashSet<_>>();
    let mut unpublished_ports = image_config
        .tcp_ports()
        .into_iter()
        .filter(|port| !published_guest_ports.contains(port))
        .collect::<Vec<_>>();
    unpublished_ports.sort_unstable();
    unpublished_ports.dedup();
    unpublished_ports
}

fn warn_unpublished_exposed_ports(image_config: &ContainerImageConfig, options: &BoxOptions) {
    let guest_ports = unpublished_exposed_tcp_ports(image_config, options);
    if !guest_ports.is_empty() {
        tracing::warn!(
            ?guest_ports,
            "Image EXPOSE declarations are metadata only; publish these guest ports explicitly \
             with BoxOptions.ports (CLI: -p GUEST_PORT) or use a network tunnel"
        );
    }
}

/// Spawn VM subprocess and return handler.
async fn spawn_vm(
    box_id: &BoxID,
    config: &InstanceSpec,
    options: &BoxOptions,
    layout: &BoxFilesystemLayout,
) -> BoxliteResult<Box<dyn VmmHandler>> {
    let mut controller = ShimController::new(
        find_binary("boxlite-shim")?,
        VmmKind::Libkrun,
        box_id.clone(),
        options.clone(),
        layout.clone(),
    )?;

    controller.start(config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::ContainerImageConfig;
    use crate::runtime::options::{PortProtocol, PortSpec};

    #[test]
    fn reports_only_exposed_tcp_ports_without_explicit_publication() {
        let image_config = ContainerImageConfig {
            exposed_ports: vec![
                "3000/tcp".to_string(),
                "8080".to_string(),
                "53/udp".to_string(),
            ],
            ..Default::default()
        };
        let options = BoxOptions {
            ports: vec![PortSpec {
                host_port: None,
                guest_port: 8080,
                protocol: PortProtocol::Tcp,
                host_ip: None,
            }],
            ..Default::default()
        };

        assert_eq!(
            unpublished_exposed_tcp_ports(&image_config, &options),
            vec![3000]
        );

        let disabled_options = BoxOptions {
            network: crate::runtime::options::NetworkSpec::Disabled,
            ..Default::default()
        };
        assert!(
            unpublished_exposed_tcp_ports(&image_config, &disabled_options).is_empty(),
            "disabled networking never created implicit EXPOSE listeners"
        );
    }

    #[test]
    fn configure_guest_rootfs_attaches_disk_read_only() {
        // The guest root must reject writes so a tenant workload that enters the
        // container by fork (without execve) cannot reopen the agent binary
        // through `/proc/<pid>/exe` for write (CVE-2019-5736). The rootfs is
        // now read-only by construction — attached as a read-only block device —
        // rather than remounted read-only after boot, so the regression guard
        // lives here, on the `read_only` attach flag.
        let disk_path = PathBuf::from("/tmp/fake-guest-rootfs.ext4");
        let rootfs = GuestRootfs::new(
            disk_path.clone(),
            Strategy::Disk {
                disk_path: disk_path.clone(),
                device_path: None,
            },
            None,
            None,
            vec![],
        )
        .unwrap();

        let mut volume_mgr = GuestVolumeManager::new();
        let configured = configure_guest_rootfs(rootfs, &mut volume_mgr).unwrap();

        // The attach happened and the strategy records the guest device path
        // (a fresh manager hands out /dev/vda first).
        match configured.strategy {
            Strategy::Disk { device_path, .. } => {
                assert_eq!(device_path.as_deref(), Some("/dev/vda"));
            }
            _ => panic!("expected disk-based strategy"),
        }

        let config = volume_mgr.build_vmm_config();
        let devices = config.block_devices.devices();
        assert_eq!(devices.len(), 1);
        assert!(
            devices[0].read_only,
            "guest rootfs must attach read-only (is_disk_read_only)"
        );
    }
}
