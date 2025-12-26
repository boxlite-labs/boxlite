use boxlite::{BoxInfo, BoxStatus};
use pyo3::prelude::*;

#[pyclass(name = "BoxInfo")]
#[derive(Clone)]
pub(crate) struct PyBoxInfo {
    #[pyo3(get)]
    pub(crate) id: String,
    #[pyo3(get)]
    pub(crate) state: String,
    #[pyo3(get)]
    pub(crate) created_at: String,
    #[pyo3(get)]
    pub(crate) pid: Option<u32>,
    #[pyo3(get)]
    pub(crate) transport: String,
    #[pyo3(get)]
    pub(crate) image: String,
    #[pyo3(get)]
    pub(crate) cpus: u8,
    #[pyo3(get)]
    pub(crate) memory_mib: u32,
}

impl From<BoxInfo> for PyBoxInfo {
    fn from(info: BoxInfo) -> Self {
        let state_str = match info.status {
            BoxStatus::Unknown => "unknown",
            BoxStatus::Starting => "starting",
            BoxStatus::Running => "running",
            BoxStatus::Stopping => "stopping",
            BoxStatus::Stopped => "stopped",
        };

        PyBoxInfo {
            id: info.id,
            state: state_str.to_string(),
            created_at: info.created_at.to_rfc3339(),
            pid: info.pid,
            transport: info.transport.to_string(),
            image: info.image,
            cpus: info.cpus,
            memory_mib: info.memory_mib,
        }
    }
}
