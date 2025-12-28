use std::sync::Arc;

use boxlite::BoxliteRuntime;
use pyo3::prelude::*;

use crate::box_handle::PyBox;
use crate::info::PyBoxInfo;
use crate::metrics::PyRuntimeMetrics;
use crate::options::{PyBoxOptions, PyOptions};
use crate::util::map_err;

#[pyclass(name = "Boxlite")]
pub(crate) struct PyBoxlite {
    pub(crate) runtime: Arc<BoxliteRuntime>,
}

#[pymethods]
impl PyBoxlite {
    #[new]
    fn new(options: PyOptions) -> PyResult<Self> {
        let runtime = BoxliteRuntime::new(options.into()).map_err(map_err)?;

        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    #[staticmethod]
    fn default() -> PyResult<Self> {
        let runtime = BoxliteRuntime::default_runtime();
        Ok(Self {
            runtime: Arc::new(runtime.clone()),
        })
    }

    #[staticmethod]
    fn init_default(options: PyOptions) -> PyResult<()> {
        BoxliteRuntime::init_default_runtime(options.into()).map_err(map_err)
    }

    #[pyo3(signature = (options, name=None))]
    fn create(&self, options: PyBoxOptions, name: Option<String>) -> PyResult<PyBox> {
        let image = options.image_ref();
        let handle = self
            .runtime
            .create(&image, options.into(), name)
            .map_err(map_err)?;

        Ok(PyBox {
            handle: Arc::new(handle),
        })
    }

    #[pyo3(signature = (_state=None))]
    fn list_info(&self, _state: Option<String>) -> PyResult<Vec<PyBoxInfo>> {
        let infos = self.runtime.list_info().map_err(map_err)?;

        Ok(infos.into_iter().map(PyBoxInfo::from).collect())
    }

    /// Get information about a specific box by ID or name.
    fn get_info(&self, id_or_name: String) -> PyResult<Option<PyBoxInfo>> {
        Ok(self
            .runtime
            .get_info(&id_or_name)
            .map_err(map_err)?
            .map(PyBoxInfo::from))
    }

    /// Get a box handle by ID or name (for reattach or restart).
    ///
    /// Args:
    ///     id_or_name: Either a box ID (ULID) or user-defined name
    ///
    /// Returns:
    ///     Box handle if found, None otherwise
    fn get(&self, id_or_name: String) -> PyResult<Option<PyBox>> {
        tracing::trace!("Python get() called with id_or_name={}", id_or_name);

        let result = self.runtime.get(&id_or_name).map_err(map_err)?;

        tracing::trace!("Rust get() returned: is_some={}", result.is_some());

        let py_box = result.map(|handle| {
            tracing::trace!("Wrapping LiteBox in PyBox for id_or_name={}", id_or_name);
            PyBox {
                handle: Arc::new(handle),
            }
        });

        tracing::trace!("Returning PyBox to Python: is_some={}", py_box.is_some());
        Ok(py_box)
    }

    fn metrics(&self) -> PyResult<PyRuntimeMetrics> {
        let metrics = self.runtime.metrics();
        Ok(PyRuntimeMetrics::from(metrics))
    }

    /// Remove a box by ID or name.
    ///
    /// Args:
    ///     id_or_name: Either a box ID (ULID) or user-defined name
    ///     force: If True, stop the box first if running (default: False)
    #[pyo3(signature = (id_or_name, force=false))]
    fn remove<'py>(
        &self,
        py: Python<'py>,
        id_or_name: String,
        force: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let runtime = Arc::clone(&self.runtime);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            runtime.remove(&id_or_name, force).await.map_err(map_err)?;
            Ok(())
        })
    }

    fn close(&self) -> PyResult<()> {
        Ok(())
    }

    fn __enter__(slf: PyRef<'_, Self>) -> PyResult<PyRef<'_, Self>> {
        Ok(slf)
    }

    fn __exit__(
        &self,
        _exc_type: Py<PyAny>,
        _exc_val: Py<PyAny>,
        _exc_tb: Py<PyAny>,
    ) -> PyResult<()> {
        self.close()
    }

    fn __repr__(&self) -> String {
        "Boxlite(open=true)".to_string()
    }
}
