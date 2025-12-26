//! Task: Filesystem setup.
//!
//! Creates box directory structure and optionally sets up the mounts/ → shared/ binding.

use super::{InitCtx, log_task_error, task_start};
use crate::litebox::init::types::{FilesystemInput, FilesystemOutput};
use crate::pipeline::PipelineTask;
use async_trait::async_trait;
use boxlite_shared::errors::BoxliteResult;

pub struct FilesystemTask;

#[async_trait]
impl PipelineTask<InitCtx> for FilesystemTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (runtime, isolate_mounts) = {
            let ctx = ctx.lock().await;
            (ctx.runtime.clone(), ctx.config.options.isolate_mounts)
        };

        let output = run_filesystem(FilesystemInput {
            box_id: &box_id,
            runtime: &runtime,
            isolate_mounts,
        })
        .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        let mut ctx = ctx.lock().await;
        ctx.guard.set_layout(output.layout.clone());
        ctx.fs_output = Some(output);

        Ok(())
    }

    fn name(&self) -> &str {
        "filesystem_setup"
    }
}

/// Create box directories and optionally set up shared filesystem binding.
///
/// Sets up:
/// 1. Box directory structure (sockets/, mounts/)
/// 2. Bind mount from mounts/ → shared/ (Linux only, when isolate_mounts=true)
fn run_filesystem(input: FilesystemInput<'_>) -> BoxliteResult<FilesystemOutput> {
    let layout = input
        .runtime
        .layout
        .box_layout(input.box_id.as_str(), input.isolate_mounts)?;

    layout.prepare()?;

    #[cfg(target_os = "linux")]
    let bind_mount = if input.isolate_mounts {
        use crate::fs::{BindMountConfig, create_bind_mount};
        let mounts_dir = layout.mounts_dir();
        let mount = create_bind_mount(
            &BindMountConfig::new(&mounts_dir, &layout.shared_dir()).read_only(),
        )?;
        Some(mount)
    } else {
        None
    };

    #[cfg(not(target_os = "linux"))]
    let _ = input.isolate_mounts;

    Ok(FilesystemOutput {
        layout,
        #[cfg(target_os = "linux")]
        bind_mount,
    })
}
