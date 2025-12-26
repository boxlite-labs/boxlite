//! Generic table-driven pipeline execution framework.
//!
//! This module provides a reusable pipeline infrastructure that supports:
//! - Table-driven execution plans based on state
//! - Parallel and sequential task execution modes
//! - Arbitrary number of stages and tasks
//!
//! ## Architecture
//!
//! ```text
//! Pipeline → Stages → Tasks
//!
//! - Pipeline: Orchestrates execution of all stages
//! - Stage: Groups related tasks with an execution mode (parallel/sequential)
//! - Task: Atomic unit of work
//! ```
//!
//! ## Example
//!
//! ```ignore
//! use pipeline::{ExecutionPlan, PipelineBuilder, PipelineExecutor, Stage};
//! use std::sync::Arc;
//! use tokio::sync::Mutex;
//!
//! struct Context;
//! struct TaskA;
//! struct TaskB;
//!
//! let plan = ExecutionPlan::new(vec![Stage::parallel(vec![
//!     Box::new(TaskA),
//!     Box::new(TaskB),
//! ])]);
//!
//! let ctx = Arc::new(Mutex::new(Context));
//! let pipeline = PipelineBuilder::from_plan(plan);
//! let metrics = PipelineExecutor::execute(pipeline, ctx).await?;
//! println!("pipeline took {}ms", metrics.total_duration_ms);
//! ```

mod metrics;
#[allow(clippy::module_inception)]
mod pipeline;
mod stage;
mod task;

pub use metrics::{PipelineMetrics, StageMetrics, TaskMetrics};
pub use pipeline::{ExecutionPlan, Pipeline, PipelineBuilder, PipelineExecutor};
pub use stage::{ExecutionMode, Stage};
pub use task::{BoxedTask, PipelineTask};
