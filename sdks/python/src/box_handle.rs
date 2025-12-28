use std::sync::Arc;

use crate::exec::PyExecution;
use crate::info::PyBoxInfo;
use crate::metrics::PyBoxMetrics;
use crate::util::map_err;
use boxlite::{BoxCommand, LiteBox};
use pyo3::prelude::*;

#[pyclass(name = "Box")]
pub(crate) struct PyBox {
    pub(crate) handle: Arc<LiteBox>,
}

#[pymethods]
impl PyBox {
    #[getter]
    fn id(&self) -> PyResult<String> {
        Ok(self.handle.id().to_string())
    }

    #[getter]
    fn name(&self) -> Option<String> {
        self.handle.name().map(|s| s.to_string())
    }

    fn info(&self) -> PyBoxInfo {
        PyBoxInfo::from(self.handle.info())
    }

    #[pyo3(signature = (command, args=None, env=None, tty=false))]
    fn exec<'a>(
        &self,
        py: Python<'a>,
        command: String,
        args: Option<Vec<String>>,
        env: Option<Vec<(String, String)>>,
        tty: bool,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);

        let args = args.unwrap_or_default();

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut cmd = BoxCommand::new(command);
            cmd = cmd.args(args);
            if let Some(env_vars) = env {
                for (k, v) in env_vars {
                    cmd = cmd.env(k, v);
                }
            }
            if tty {
                // Auto-detect terminal size like Docker (done inside .tty())
                cmd = cmd.tty(true);
            }

            let execution = handle.exec(cmd).await.map_err(map_err)?;

            Ok(PyExecution {
                execution: Arc::new(execution),
            })
        })
    }

    /// Stop the box (preserves state for restart).
    fn stop<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            handle.stop().await.map_err(map_err)?;
            Ok(())
        })
    }

    fn metrics<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let metrics = handle.metrics().await.map_err(map_err)?;
            Ok(PyBoxMetrics::from(metrics))
        })
    }

    fn __aenter__<'a>(slf: PyRefMut<'_, Self>, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&slf.handle);

        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(PyBox { handle }) })
    }

    #[allow(unsafe_op_in_unsafe_fn)]
    fn __aexit__<'a>(
        slf: PyRefMut<'a, Self>,
        py: Python<'a>,
        _exc_type: Py<PyAny>,
        _exc_val: Py<PyAny>,
        _exc_tb: Py<PyAny>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&slf.handle);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            handle.stop().await.map_err(map_err)?;
            Ok(())
        })
    }

    fn __repr__(&self) -> String {
        format!("Box(id={:?})", self.handle.id().to_string())
    }
}
