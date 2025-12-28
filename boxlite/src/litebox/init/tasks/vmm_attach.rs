//! Task: VMM Attach - Attach to an existing running VM process.
//!
//! Creates a handler for an already-running VM subprocess by PID.
//! Used for reconnecting to detached boxes.

use super::{InitCtx, task_start};
use crate::pipeline::PipelineTask;
use crate::vmm::controller::ShimHandler;
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct VmmAttachTask;

#[async_trait]
impl PipelineTask<InitCtx> for VmmAttachTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (runtime, config_id) = {
            let ctx = ctx.lock().await;
            (ctx.runtime.clone(), ctx.config.id.clone())
        };

        // Load state from box_manager to get PID
        let (_config, state) = runtime
            .box_manager
            .box_by_id(&config_id)?
            .ok_or_else(|| BoxliteError::NotFound(config_id.clone()))?;

        let pid = state
            .pid
            .ok_or_else(|| BoxliteError::InvalidState("Running box has no PID".into()))?;

        // Verify process is still alive
        if !crate::util::is_process_alive(pid) {
            return Err(BoxliteError::InvalidState(
                "Box process is no longer running".into(),
            ));
        }

        // Attach to existing process (no log_handler for reconnect)
        let handler = ShimHandler::from_pid(pid, box_id.clone());

        let mut ctx = ctx.lock().await;
        ctx.guard.set_handler(Box::new(handler));

        tracing::info!(
            box_id = %box_id,
            pid = pid,
            "Attached to existing VM process"
        );

        Ok(())
    }

    fn name(&self) -> &str {
        "vmm_attach"
    }
}
