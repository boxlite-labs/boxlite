//! Task: Guest initialization.
//!
//! Sends init configuration to guest and starts container.
//! Builds guest volumes from volume manager, uses rootfs config from vmm_config stage.

use super::{InitCtx, log_task_error, task_start};
use crate::images::ContainerImageConfig;
use crate::pipeline::PipelineTask;
use crate::portal::GuestSession;
use crate::portal::interfaces::{ContainerRootfsInitConfig, GuestInitConfig, NetworkInitConfig};
use crate::runtime::types::ContainerId;
use crate::volumes::{ContainerMount, GuestVolumeManager};
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct GuestInitTask;

#[async_trait]
impl PipelineTask<InitCtx> for GuestInitTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (
            guest_session,
            container_image_config,
            container_id,
            volume_mgr,
            rootfs_init,
            container_mounts,
        ) =
            {
                let mut ctx = ctx.lock().await;
                let guest_session = ctx
                    .guest_session
                    .take()
                    .ok_or_else(|| BoxliteError::Internal("connect task must run first".into()))?;
                let container_image_config = ctx
                    .container_image_config
                    .clone()
                    .ok_or_else(|| BoxliteError::Internal("rootfs task must run first".into()))?;
                let volume_mgr = ctx.volume_mgr.take().ok_or_else(|| {
                    BoxliteError::Internal("vmm_spawn task must run first".into())
                })?;
                let rootfs_init = ctx.rootfs_init.take().ok_or_else(|| {
                    BoxliteError::Internal("vmm_spawn task must run first".into())
                })?;
                let container_mounts = ctx.container_mounts.take().ok_or_else(|| {
                    BoxliteError::Internal("vmm_spawn task must run first".into())
                })?;
                (
                    guest_session,
                    container_image_config,
                    ctx.config.container.id.clone(),
                    volume_mgr,
                    rootfs_init,
                    container_mounts,
                )
            };

        run_guest_init(
            guest_session.clone(),
            &container_image_config,
            &container_id,
            &volume_mgr,
            &rootfs_init,
            &container_mounts,
        )
        .await
        .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        let mut ctx = ctx.lock().await;
        ctx.guest_session = Some(guest_session);
        ctx.volume_mgr = Some(volume_mgr);
        ctx.rootfs_init = Some(rootfs_init);
        ctx.container_mounts = Some(container_mounts);

        Ok(())
    }

    fn name(&self) -> &str {
        "guest_init"
    }
}

/// Initialize guest and start container.
async fn run_guest_init(
    guest_session: GuestSession,
    container_image_config: &ContainerImageConfig,
    container_id: &ContainerId,
    volume_mgr: &GuestVolumeManager,
    rootfs_init: &ContainerRootfsInitConfig,
    container_mounts: &[ContainerMount],
) -> BoxliteResult<()> {
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

    // Step 2: Container Init (rootfs + container image config + user volume mounts)
    tracing::info!("Sending container configuration to guest");
    let mut container_interface = guest_session.container().await?;
    let returned_id = container_interface
        .init(
            container_id_str,
            container_image_config.clone(),
            rootfs_init.clone(),
            container_mounts.to_vec(),
        )
        .await?;
    tracing::info!(container_id = %returned_id, "Container initialized");

    Ok(())
}
