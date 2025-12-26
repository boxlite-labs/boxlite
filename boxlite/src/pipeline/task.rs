//! Generic task trait for pipeline execution.

use async_trait::async_trait;
use boxlite_shared::errors::BoxliteResult;

/// Trait for tasks that can be executed in a pipeline.
///
/// Implement this trait to define custom task types for your pipeline.
/// Tasks run with a shared context, which is cloned per task.
#[async_trait]
pub trait PipelineTask<Ctx>: Send + Sync {
    /// Execute the task with the shared pipeline context.
    async fn run(self: Box<Self>, ctx: Ctx) -> BoxliteResult<()>;

    /// Get human-readable task name for logging.
    fn name(&self) -> &str;
}

pub type BoxedTask<Ctx> = Box<dyn PipelineTask<Ctx>>;
