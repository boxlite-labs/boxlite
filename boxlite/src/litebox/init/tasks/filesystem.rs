//! Task: Filesystem setup.
//!
//! Creates box directory structure and optionally sets up the mounts/ → shared/ binding.

use super::{InitCtx, log_task_error, task_start};
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

        let layout = runtime
            .layout
            .box_layout(&box_id, isolate_mounts)
            .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        layout
            .prepare()
            .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        #[cfg(target_os = "linux")]
        let bind_mount = if isolate_mounts {
            use crate::fs::{BindMountConfig, create_bind_mount};
            let mount = create_bind_mount(
                &BindMountConfig::new(&layout.mounts_dir(), &layout.shared_dir()).read_only(),
            )
            .inspect_err(|e| log_task_error(&box_id, task_name, e))?;
            Some(mount)
        } else {
            None
        };

        let mut ctx = ctx.lock().await;
        ctx.guard.set_layout(layout.clone());
        ctx.layout = Some(layout);
        #[cfg(target_os = "linux")]
        {
            ctx.bind_mount = bind_mount;
        }

        Ok(())
    }

    fn name(&self) -> &str {
        "filesystem_setup"
    }
}
