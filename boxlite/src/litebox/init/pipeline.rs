//! Initialization pipeline with RAII cleanup.

use super::stages;
use super::types::*;
use crate::BoxID;
use crate::controller::ShimController;
use crate::metrics::BoxMetricsStorage;
use crate::runtime::RuntimeInner;
use crate::runtime::initrf::InitRootfs;
use crate::runtime::layout::BoxFilesystemLayout;
use crate::runtime::options::BoxOptions;
use crate::vmm::VmmController;
use boxlite_shared::errors::BoxliteResult;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::OnceCell;

/// RAII guard for cleanup on initialization failure.
///
/// Automatically cleans up resources and increments failure counter
/// if dropped without being disarmed.
pub struct CleanupGuard {
    runtime: RuntimeInner,
    layout: Option<BoxFilesystemLayout>,
    controller: Option<ShimController>,
    armed: bool,
}

impl CleanupGuard {
    pub fn new(runtime: RuntimeInner) -> Self {
        Self {
            runtime,
            layout: None,
            controller: None,
            armed: true,
        }
    }

    /// Register layout for cleanup on failure.
    pub fn set_layout(&mut self, layout: BoxFilesystemLayout) {
        self.layout = Some(layout);
    }

    /// Register controller for cleanup on failure.
    pub fn set_controller(&mut self, controller: ShimController) {
        self.controller = Some(controller);
    }

    /// Take ownership of controller (for success path).
    pub fn take_controller(&mut self) -> Option<ShimController> {
        self.controller.take()
    }

    /// Disarm the guard (call on success).
    ///
    /// After disarming, Drop will not perform cleanup.
    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        tracing::warn!("Box initialization failed, cleaning up");

        // Stop controller if started
        if let Some(ref mut controller) = self.controller
            && let Err(e) = controller.stop()
        {
            tracing::warn!("Failed to stop controller during cleanup: {}", e);
        }

        // Cleanup filesystem
        if let Some(ref layout) = self.layout
            && let Err(e) = layout.cleanup()
        {
            tracing::warn!("Failed to cleanup box directory: {}", e);
        }

        // Increment failure counter
        self.runtime
            .non_sync_state
            .runtime_metrics
            .boxes_failed
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Initialization pipeline executor.
///
/// Orchestrates all initialization stages with automatic cleanup on failure.
pub struct InitPipeline {
    box_id: BoxID,
    home_dir: PathBuf,
    options: BoxOptions,
    runtime: RuntimeInner,
    init_rootfs_cell: Arc<OnceCell<InitRootfs>>,
}

impl InitPipeline {
    pub fn new(
        box_id: BoxID,
        home_dir: PathBuf,
        options: BoxOptions,
        runtime: RuntimeInner,
        init_rootfs_cell: Arc<OnceCell<InitRootfs>>,
    ) -> Self {
        Self {
            box_id,
            home_dir,
            options,
            runtime,
            init_rootfs_cell,
        }
    }

    /// Execute all initialization stages.
    ///
    /// Returns `BoxInner` on success, or error with automatic cleanup.
    pub async fn run(self) -> BoxliteResult<BoxInner> {
        use std::time::Instant;

        let total_start = Instant::now();

        // Create cleanup guard (armed by default)
        let mut guard = CleanupGuard::new(self.runtime.clone());

        // ====================================================================
        // PARALLEL PHASE: Stages 1-3 have no interdependencies
        // ====================================================================
        let (fs_result, rootfs_result, init_result) = tokio::join!(
            // Stage 1: Filesystem setup
            async {
                let start = Instant::now();
                let result = stages::filesystem::run(FilesystemInput {
                    box_id: &self.box_id,
                    runtime: &self.runtime,
                });
                (result, start.elapsed().as_millis())
            },
            // Stage 2: Rootfs preparation (pulls image)
            async {
                let start = Instant::now();
                let result = stages::rootfs::run(RootfsInput {
                    options: &self.options,
                    runtime: &self.runtime,
                })
                .await;
                (result, start.elapsed().as_millis())
            },
            // Stage 3: Init image (lazy initialization)
            async {
                let start = Instant::now();
                let result = stages::init_image::run(InitImageInput {
                    runtime: &self.runtime,
                    init_rootfs_cell: &self.init_rootfs_cell,
                })
                .await;
                (result, start.elapsed().as_millis())
            },
        );

        // Extract outputs and durations from parallel phase (propagate errors)
        let (fs_output, stage_filesystem_setup_ms) = fs_result;
        let fs_output = fs_output.inspect_err(|e| {
            tracing::error!(box_id = %self.box_id, stage = "filesystem", "Box init failed: {}", e);
        })?;

        let (rootfs_output, stage_image_prepare_ms) = rootfs_result;
        let rootfs_output = rootfs_output.inspect_err(|e| {
            tracing::error!(box_id = %self.box_id, stage = "rootfs", "Box init failed: {}", e);
        })?;

        let (init_output, stage_init_rootfs_ms) = init_result;
        let init_output = init_output.inspect_err(|e| {
            tracing::error!(box_id = %self.box_id, stage = "init_image", "Box init failed: {}", e);
        })?;

        // Register layout for cleanup (after parallel phase succeeds)
        guard.set_layout(fs_output.layout.clone());

        // ====================================================================
        // SEQUENTIAL PHASE: Stages 4-6 depend on previous outputs
        // ====================================================================

        // Stage 4: Config construction
        let stage4_start = Instant::now();
        let config_output = stages::config::run(ConfigInput {
            options: &self.options,
            layout: &fs_output.layout,
            rootfs: &rootfs_output,
            init_rootfs: &init_output.init_rootfs,
            home_dir: &self.home_dir,
        })
        .await
        .inspect_err(|e| {
            tracing::error!(box_id = %self.box_id, stage = "config", "Box init failed: {}", e);
        })?;
        let stage_box_config_ms = stage4_start.elapsed().as_millis();

        // Stage 5: Box spawn
        let stage5_start = Instant::now();
        let spawn_output = stages::spawn::run(SpawnInput {
            box_id: &self.box_id,
            config: &config_output.box_config,
        })
        .await
        .inspect_err(|e| {
            tracing::error!(box_id = %self.box_id, stage = "spawn", "Box init failed: {}", e);
        })?;
        let stage_box_spawn_ms = stage5_start.elapsed().as_millis();

        // Update manager with PID and state
        if let Some(pid) = spawn_output.controller.pid()
            && let Ok(state) = self.runtime.acquire_write()
        {
            let _ = state.box_manager.update_pid(&self.box_id, Some(pid));
            let _ = state
                .box_manager
                .update_state(&self.box_id, crate::management::BoxState::Running);
        }

        guard.set_controller(spawn_output.controller);

        // Stage 6: Guest initialization
        let stage6_start = Instant::now();
        let guest_output = stages::guest::run(GuestInput {
            guest_session: spawn_output.guest_session,
            rootfs_result: rootfs_output.rootfs_result,
            container_config: rootfs_output.container_config,
            is_cow_child: config_output.is_cow_child,
            user_volumes: config_output.user_volumes,
        })
        .await
        .inspect_err(|e| {
            tracing::error!(box_id = %self.box_id, stage = "guest", "Box init failed: {}", e);
        })?;
        let stage_container_init_ms = stage6_start.elapsed().as_millis();

        // ====================================================================
        // SUCCESS: Calculate metrics and assemble result
        // ====================================================================
        let total_create_duration_ms = total_start.elapsed().as_millis();
        let controller = guard.take_controller().expect("controller was set");
        let guest_boot_duration_ms = controller.guest_boot_duration_ms();

        let mut metrics = BoxMetricsStorage::new();
        metrics.set_total_create_duration(total_create_duration_ms);
        if let Some(boot_ms) = guest_boot_duration_ms {
            metrics.set_guest_boot_duration(boot_ms);
        }

        // Set stage durations
        metrics.set_stage_filesystem_setup(stage_filesystem_setup_ms);
        metrics.set_stage_image_prepare(stage_image_prepare_ms);
        metrics.set_stage_init_rootfs(stage_init_rootfs_ms);
        metrics.set_stage_box_config(stage_box_config_ms);
        metrics.set_stage_box_spawn(stage_box_spawn_ms);
        metrics.set_stage_container_init(stage_container_init_ms);

        tracing::debug!(
            total_create_duration_ms,
            stage_filesystem_setup_ms,
            stage_image_prepare_ms,
            stage_init_rootfs_ms,
            stage_box_config_ms,
            stage_box_spawn_ms,
            stage_container_init_ms,
            "Box initialization stages completed"
        );

        // Disarm guard before assembling result
        guard.disarm();

        // Assemble final state
        let image_for_disk_install = if config_output.is_cow_child {
            None
        } else {
            Some(rootfs_output.image)
        };

        Ok(BoxInner {
            box_home: fs_output.layout.root().to_path_buf(),
            controller: std::sync::Mutex::new(Box::new(controller)),
            guest_session: guest_output.guest_session,
            network_backend: config_output.network_backend,
            metrics,
            disk: config_output.disk,
            rootfs_disk: config_output.rootfs_disk,
            init_disk: config_output.init_disk,
            image_for_disk_install,
            container_id: guest_output.container_id,
        })
    }
}
