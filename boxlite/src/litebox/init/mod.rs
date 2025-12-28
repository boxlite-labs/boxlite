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

pub(crate) use crate::litebox::box_impl::LiveState;

use crate::litebox::BoxStatus;
use crate::litebox::config::BoxConfig;
use crate::metrics::BoxMetricsStorage;
use crate::pipeline::{
    BoxedTask, ExecutionPlan, PipelineBuilder, PipelineExecutor, PipelineMetrics, Stage,
};
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::runtime::types::BoxState;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::sync::Arc;
use tokio::sync::Mutex;

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

    /// Build and initialize LiveState.
    ///
    /// Executes all initialization stages with automatic cleanup on failure.
    /// Returns LiveState which will be set in BoxImpl.
    pub(crate) async fn build(self) -> BoxliteResult<LiveState> {
        use std::time::Instant;

        let total_start = Instant::now();

        let BoxBuilder {
            runtime,
            config,
            state,
        } = self;

        let status = state.status;
        let reuse_rootfs = status == BoxStatus::Stopped;
        let skip_guest_wait = status == BoxStatus::Running;

        let ctx = InitPipelineContext::new(config, runtime.clone(), reuse_rootfs, skip_guest_wait);
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

        metrics.log_init_stages();

        ctx.guard.disarm();

        // Get guest_session from GuestConnectTask
        let guest_session = ctx
            .guest_session
            .take()
            .ok_or_else(|| BoxliteError::Internal("guest_connect task must run first".into()))?;

        // Get disks from context (for Running, create disk reference directly)
        let (container_disk, guest_disk) = if status == BoxStatus::Running {
            // Reattach: create disk reference to existing qcow2
            use crate::disk::DiskFormat;
            let disk = crate::disk::Disk::new(
                ctx.config.box_home.join("root.qcow2"),
                DiskFormat::Qcow2,
                true,
            );
            (disk, None)
        } else {
            // Starting/Stopped: get disks from rootfs tasks
            let container_disk = ctx
                .container_disk
                .take()
                .ok_or_else(|| BoxliteError::Internal("rootfs task must run first".into()))?;
            (container_disk, ctx.guest_disk.take())
        };

        #[cfg(target_os = "linux")]
        let bind_mount = ctx.bind_mount.take();

        // Build LiveState
        Ok(LiveState::new(
            handler,
            guest_session,
            metrics,
            container_disk,
            guest_disk,
            #[cfg(target_os = "linux")]
            bind_mount,
        ))
    }
}
