//! Box initialization orchestration.
//!
//! ## Architecture
//!
//! Initialization is table-driven with different execution plans based on BoxStatus:
//!
//! ```text
//! Starting (new box):
//!   1. Filesystem           (create layout)
//!   2. ContainerRootfs ─┬─  (pull image, create COW disk)
//!      GuestRootfs     ─┘   (prepare guest, create COW disk)
//!   3. VmmSpawn             (build config + spawn VM)
//!   4. GuestConnect         (wait for guest ready)
//!   5. GuestInit            (initialize container)
//!
//! Stopped (restart):
//!   1. Filesystem           (load existing layout)
//!   2. ContainerRootfs ─┬─  (reuse existing COW disk - preserves user data)
//!      GuestRootfs     ─┘   (reuse existing COW disk)
//!   3. VmmSpawn             (build config + spawn NEW VM)
//!   4. GuestConnect         (wait for guest ready)
//!   5. GuestInit            (re-initialize container in new VM)
//!
//! Running (reattach):
//!   1. VmmAttach            (attach to running VM)
//!   2. GuestConnect         (reconnect to guest)
//! ```
//!
//! `CleanupGuard` provides RAII cleanup on failure.

mod tasks;
mod types;

pub(crate) use crate::litebox::box_impl::BoxImpl;

use crate::litebox::BoxStatus;
use crate::litebox::config::BoxConfig;
use crate::metrics::BoxMetricsStorage;
use crate::pipeline::{
    BoxedTask, ExecutionPlan, PipelineBuilder, PipelineExecutor, PipelineMetrics, Stage,
};
use crate::runtime::guest_rootfs::GuestRootfs;
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::runtime::types::{BoxState, ContainerId};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell};

use tasks::{
    ContainerRootfsTask, FilesystemTask, GuestConnectTask, GuestInitTask, GuestRootfsTask, InitCtx,
    VmmAttachTask, VmmSpawnTask,
};
use types::InitPipelineContext;

// ============================================================================
// EXECUTION PLAN
// ============================================================================

/// Get execution plan based on BoxStatus.
fn get_execution_plan(status: BoxStatus) -> ExecutionPlan<InitCtx> {
    let stages: Vec<Stage<BoxedTask<InitCtx>>> = match status {
        BoxStatus::Starting => vec![
            // Phase 1: Setup filesystem layout first
            Stage::sequential(vec![Box::new(FilesystemTask)]),
            // Phase 2: Prepare rootfs (now has access to layout for disk paths)
            Stage::parallel(vec![
                Box::new(ContainerRootfsTask),
                Box::new(GuestRootfsTask),
            ]),
            // Phase 3: Build config and spawn VM
            Stage::sequential(vec![Box::new(VmmSpawnTask)]),
            // Phase 4: Connect to guest and initialize container
            Stage::sequential(vec![Box::new(GuestConnectTask)]),
            Stage::sequential(vec![Box::new(GuestInitTask)]),
        ],
        BoxStatus::Stopped => vec![
            // Restart: Same flow but rootfs tasks reuse existing COW disks
            // (preserves user modifications from previous run)
            Stage::sequential(vec![Box::new(FilesystemTask)]),
            Stage::parallel(vec![
                Box::new(ContainerRootfsTask),
                Box::new(GuestRootfsTask),
            ]),
            Stage::sequential(vec![Box::new(VmmSpawnTask)]),
            Stage::sequential(vec![Box::new(GuestConnectTask)]),
            // GuestInit must run - new VM process has fresh guest daemon
            Stage::sequential(vec![Box::new(GuestInitTask)]),
        ],
        BoxStatus::Running => vec![
            // Reattach: Attach to existing VM process and connect to guest
            Stage::sequential(vec![Box::new(VmmAttachTask)]),
            Stage::sequential(vec![Box::new(GuestConnectTask)]),
        ],
        _ => panic!("Invalid BoxStatus for initialization: {:?}", status),
    };

    ExecutionPlan::new(stages)
}

fn box_metrics_from_pipeline(pipeline_metrics: &PipelineMetrics) -> BoxMetricsStorage {
    let mut metrics = BoxMetricsStorage::new();

    if let Some(duration_ms) = pipeline_metrics.task_duration_ms("filesystem_setup") {
        metrics.set_stage_filesystem_setup(duration_ms);
    }
    if let Some(duration_ms) = pipeline_metrics.task_duration_ms("container_rootfs_prep") {
        metrics.set_stage_image_prepare(duration_ms);
    }
    if let Some(duration_ms) = pipeline_metrics.task_duration_ms("guest_rootfs_init") {
        metrics.set_stage_guest_rootfs(duration_ms);
    }
    if let Some(duration_ms) = pipeline_metrics.task_duration_ms("vmm_spawn") {
        metrics.set_stage_box_spawn(duration_ms);
    }
    if let Some(duration_ms) = pipeline_metrics.task_duration_ms("vmm_attach") {
        metrics.set_stage_box_spawn(duration_ms);
    }
    if let Some(_duration_ms) = pipeline_metrics.task_duration_ms("guest_connect") {
        // Track guest connection time
        // Could add a new metric field if needed
    }
    if let Some(duration_ms) = pipeline_metrics.task_duration_ms("guest_init") {
        metrics.set_stage_container_init(duration_ms);
    }

    metrics
}

/// Builds and initializes box components.
///
/// # Example
///
/// ```ignore
/// let inner = BoxBuilder::new(runtime, config, &state)
///     .build()
///     .await?;
/// ```
pub(crate) struct BoxBuilder {
    runtime: SharedRuntimeImpl,
    config: BoxConfig,
    state: BoxState,
}

impl BoxBuilder {
    /// Create a new builder from config and state.
    ///
    /// The state determines initialization mode:
    /// - `Starting`: normal init (pull image or use rootfs path)
    /// - `Stopped`: restart (reuse existing rootfs at box_home/rootfs)
    ///
    /// # Arguments
    ///
    /// * `runtime` - Runtime providing resources (layout, guest_rootfs, etc.)
    /// * `config` - Box configuration (immutable after creation)
    /// * `state` - Current box state (determines init mode)
    pub(crate) fn new(
        runtime: SharedRuntimeImpl,
        config: BoxConfig,
        state: BoxState,
    ) -> BoxliteResult<Self> {
        // Get options reference from config (no reconstruction needed!)
        let options = &config.options;
        options.sanitize()?;

        Ok(Self {
            runtime,
            config,
            state,
        })
    }

    /// Build and initialize BoxImpl.
    ///
    /// Executes all initialization stages with automatic cleanup on failure.
    pub(crate) async fn build(self) -> BoxliteResult<BoxImpl> {
        use std::time::Instant;

        let total_start = Instant::now();

        let BoxBuilder {
            runtime,
            config,
            state,
        } = self;

        let status = state.status;

        let container_id = ContainerId::new();
        tracing::debug!(container_id = %container_id.short(), "Generated container ID");

        let home_dir = runtime.layout.home_dir().to_path_buf();
        let guest_rootfs_cell: Arc<OnceCell<GuestRootfs>> = Arc::clone(&runtime.guest_rootfs);

        let ctx = InitPipelineContext::new(
            config,
            state,
            home_dir,
            runtime,
            guest_rootfs_cell,
            container_id,
        );
        let ctx = Arc::new(Mutex::new(ctx));

        if status != BoxStatus::Starting {
            let mut ctx_guard = ctx.lock().await;
            ctx_guard.guard.disarm();
        }

        let plan = get_execution_plan(status);
        let pipeline = PipelineBuilder::from_plan(plan);
        let pipeline_metrics = PipelineExecutor::execute(pipeline, Arc::clone(&ctx)).await?;

        let mut ctx = ctx.lock().await;
        let total_create_duration_ms = total_start.elapsed().as_millis();
        let handler = ctx
            .guard
            .take_handler()
            .ok_or_else(|| BoxliteError::Internal("handler was not set".into()))?;

        let mut metrics = box_metrics_from_pipeline(&pipeline_metrics);
        metrics.set_total_create_duration(total_create_duration_ms);
        // Note: guest_boot_duration is now logged in ShimController::start(),
        // but not tracked in BoxMetrics since handler doesn't store timing metadata

        metrics.log_init_stages();

        ctx.guard.disarm();

        // Get guest_output from GuestInitTask (runs for both Starting and Stopped)
        // Reattach (Running) uses a different path and doesn't call build()
        let guest_output = ctx
            .guest_output
            .take()
            .ok_or_else(|| BoxliteError::Internal("guest_init task must run first".into()))?;

        // Update container_id in database
        if let Ok(mut state) = ctx.runtime.box_manager.update_box(&ctx.config.id) {
            state.container_id = Some(guest_output.container_id.clone());
            let _ = ctx.runtime.box_manager.save_box(&ctx.config.id, &state);
        }

        #[cfg(target_os = "linux")]
        let fs_output = ctx
            .fs_output
            .take()
            .ok_or_else(|| BoxliteError::Internal("filesystem task must run first".into()))?;
        let config_output = ctx
            .config_output
            .take()
            .ok_or_else(|| BoxliteError::Internal("vmm_config task must run first".into()))?;

        // Build final BoxImpl with all resources
        // Update state to Running now that initialization is complete
        let mut state = ctx.state.clone();
        state.set_status(BoxStatus::Running);

        Ok(BoxImpl::new(
            ctx.config.clone(),
            state,
            Arc::clone(&ctx.runtime),
            handler,
            guest_output.guest_session,
            metrics,
            config_output.disk,
            config_output.init_disk,
            guest_output.container_id.to_string(),
            #[cfg(target_os = "linux")]
            fs_output.bind_mount,
        ))
    }
}
