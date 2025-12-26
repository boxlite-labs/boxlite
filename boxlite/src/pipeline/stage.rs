//! Stage definition for table-driven pipeline execution.

/// Execution mode for a stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Execute tasks in parallel using tokio::join!
    Parallel,
    /// Execute tasks sequentially, one after another
    Sequential,
}

/// A stage contains multiple tasks and an execution mode.
///
/// Stages are executed in order, and each stage's tasks are executed
/// according to the stage's execution mode (parallel or sequential).
///
/// Generic over task type T to allow different pipeline implementations.
#[derive(Debug, Clone)]
pub struct Stage<T> {
    pub tasks: Vec<T>,
    pub execution: ExecutionMode,
}

impl<T> Stage<T> {
    /// Create a stage with parallel task execution.
    pub fn parallel(tasks: Vec<T>) -> Self {
        Self {
            tasks,
            execution: ExecutionMode::Parallel,
        }
    }

    /// Create a stage with sequential task execution.
    pub fn sequential(tasks: Vec<T>) -> Self {
        Self {
            tasks,
            execution: ExecutionMode::Sequential,
        }
    }
}
