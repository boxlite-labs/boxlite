//! Task: Guest initialization.
//!
//! Sends init configuration to guest and starts container.
//! Builds guest volumes from volume manager, uses rootfs config from vmm_config stage.

use super::{InitCtx, log_task_error, task_start};
use crate::litebox::init::types::{GuestInput, GuestOutput};
use crate::pipeline::PipelineTask;
use crate::portal::interfaces::{GuestInitConfig, NetworkInitConfig};
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct GuestInitTask;

#[async_trait]
impl PipelineTask<InitCtx> for GuestInitTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (guest_session, config_output, container_config, container_id) = {
            let mut ctx = ctx.lock().await;
            let guest_session = ctx
                .guest_session
                .take()
                .ok_or_else(|| BoxliteError::Internal("spawn task must run first".into()))?;
            let config_output = ctx
                .config_output
                .take()
                .ok_or_else(|| BoxliteError::Internal("vmm_config task must run first".into()))?;
            let container_config = ctx.container_config.clone().ok_or_else(|| {
                BoxliteError::Internal("container_config not set by VmmSpawnTask".into())
            })?;
            (
                guest_session,
                config_output,
                container_config,
                ctx.container_id.clone(),
            )
        };

        let output = run_guest_init(GuestInput {
            guest_session,
            container_config,
            container_id,
            volume_mgr: config_output.volume_mgr.clone(),
            rootfs_init: config_output.rootfs_init.clone(),
            container_mounts: config_output.container_mounts.clone(),
        })
        .await
        .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        let mut ctx = ctx.lock().await;
        ctx.guest_output = Some(output);
        ctx.config_output = Some(config_output);

        Ok(())
    }

    fn name(&self) -> &str {
        "guest_init"
    }
}

/// Initialize guest and start container.
///
/// - Guest.Init: mounts volumes (built from volume_mgr), configures network
/// - Container.Init: prepares rootfs, creates OCI container
async fn run_guest_init(input: GuestInput) -> BoxliteResult<GuestOutput> {
    let GuestInput {
        guest_session,
        container_config,
        container_id,
        volume_mgr,
        rootfs_init,
        container_mounts,
    } = input;
    let container_id_str = container_id.as_str();

    // Build guest volumes from volume manager
    let guest_volumes = volume_mgr.build_guest_mounts();

    let guest_init_config = GuestInitConfig {
        volumes: guest_volumes,
        network: Some(NetworkInitConfig {
            interface: "eth0".to_string(),
            ip: Some("192.168.127.2/24".to_string()),
            gateway: Some("192.168.127.1".to_string()),
        }),
    };

    // Step 1: Guest Init (volumes + network)
    tracing::info!("Sending guest initialization request");
    let mut guest_interface = guest_session.guest().await?;
    guest_interface.init(guest_init_config).await?;
    tracing::info!("Guest initialized successfully");

    // Step 2: Container Init (rootfs + container config + user volume mounts)
    tracing::info!("Sending container configuration to guest");
    let mut container_interface = guest_session.container().await?;
    let returned_id = container_interface
        .init(
            container_id_str,
            container_config,
            rootfs_init,
            container_mounts,
        )
        .await?;
    tracing::info!(container_id = %returned_id, "Container initialized");

    Ok(GuestOutput {
        container_id, // Use the original ContainerId (same value as returned)
        guest_session,
    })
}
