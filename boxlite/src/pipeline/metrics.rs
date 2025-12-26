use crate::pipeline::ExecutionMode;

#[derive(Debug, Clone)]
pub struct TaskMetrics {
    pub name: String,
    pub duration_ms: u128,
}

#[derive(Debug, Clone)]
pub struct StageMetrics {
    pub index: usize,
    pub execution: ExecutionMode,
    pub duration_ms: u128,
    pub tasks: Vec<TaskMetrics>,
}

#[derive(Debug, Clone)]
pub struct PipelineMetrics {
    pub total_duration_ms: u128,
    pub stages: Vec<StageMetrics>,
}

impl PipelineMetrics {
    pub fn task_duration_ms(&self, name: &str) -> Option<u128> {
        self.stages
            .iter()
            .flat_map(|stage| stage.tasks.iter())
            .find(|task| task.name == name)
            .map(|task| task.duration_ms)
    }
}
