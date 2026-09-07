use std::sync::Arc;

use crate::exec::PyExecution;
use crate::info::PyBoxInfo;
use crate::metrics::PyBoxMetrics;
use crate::network::PyNetworkHandle;
use crate::snapshot_options::{PyCloneOptions, PyExportOptions};
use crate::snapshots::PySnapshotHandle;
use crate::util::map_err;
use boxlite::{BoxCommand, CloneOptions, ExportOptions, LiteBox};
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

    fn info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let info = handle.info().await.map_err(map_err)?;
            Ok(PyBoxInfo::from(info))
        })
    }

    /// Get the snapshot handle for snapshot operations.
    ///
    /// Usage: `box.snapshot.create("name")`, `box.snapshot.list()`, etc.
    #[getter]
    fn snapshot(&self) -> PySnapshotHandle {
        PySnapshotHandle {
            handle: Arc::clone(&self.handle),
        }
    }

    /// Get the network handle for this box.
    #[getter]
    fn network(&self) -> PyNetworkHandle {
        PyNetworkHandle {
            handle: Arc::clone(&self.handle),
        }
    }

    #[pyo3(signature = (command, args=None, env=None, tty=false, user=None, timeout_secs=None, cwd=None))]
    #[allow(clippy::too_many_arguments)]
    fn exec<'a>(
        &self,
        py: Python<'a>,
        command: String,
        args: Option<Vec<String>>,
        env: Option<Vec<(String, String)>>,
        tty: bool,
        user: Option<String>,
        timeout_secs: Option<f64>,
        cwd: Option<String>,
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
                cmd = cmd.tty(true);
            }
            if let Some(user) = user {
                cmd = cmd.user(user);
            }
            if let Some(secs) = timeout_secs {
                cmd = cmd.timeout_seconds(secs).map_err(map_err)?;
            }
            if let Some(cwd) = cwd {
                cmd = cmd.working_dir(cwd);
            }

            let execution = handle.exec(cmd).await.map_err(map_err)?;

            Ok(PyExecution {
                execution: Arc::new(execution),
            })
        })
    }

    /// Attach to a session in the box (docker-py shape):
    /// - no argument → the box's main command session (`run`'s COMMAND
    ///   runs as the container init; this follows it).
    /// - with `execution_id` → reattach to that exec session on a fresh
    ///   WebSocket; the caller discards any previous handle for the same
    ///   id. Raises if the server reports the session is no longer
    ///   attachable.
    /// - `stdin=False` → read-only attach (docker's `--no-stdin`). The
    ///   returned execution has no stdin, and the server refuses writes on
    ///   the socket rather than trusting the client.
    #[pyo3(signature = (execution_id=None, stdin=true))]
    fn attach<'a>(
        &self,
        py: Python<'a>,
        execution_id: Option<String>,
        stdin: bool,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut options = match execution_id {
                Some(id) => boxlite::AttachOptions::execution(id),
                None => boxlite::AttachOptions::main(),
            };
            if !stdin {
                options = options.read_only();
            }
            let execution = handle.attach(options).await.map_err(map_err)?;
            Ok(PyExecution {
                execution: Arc::new(execution),
            })
        })
    }

    /// Start the box (initialize VM).
    fn start<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            handle.start().await.map_err(map_err)?;
            Ok(())
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

    /// Export this box as a portable `.boxlite` archive.
    #[pyo3(signature = (*, options=None, dest))]
    fn export<'a>(
        &self,
        py: Python<'a>,
        options: Option<PyExportOptions>,
        dest: String,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);
        let options: ExportOptions = options.map(Into::into).unwrap_or_default();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let archive = handle
                .export(options, std::path::Path::new(&dest))
                .await
                .map_err(map_err)?;
            Ok(archive.path().to_string_lossy().to_string())
        })
    }

    /// Clone this box, creating a new box with copied disks.
    #[pyo3(signature = (*, options=None, name=None))]
    fn clone_box<'a>(
        &self,
        py: Python<'a>,
        options: Option<PyCloneOptions>,
        name: Option<String>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);
        let options: CloneOptions = options.map(Into::into).unwrap_or_default();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let cloned = handle.clone_box(options, name).await.map_err(map_err)?;
            Ok(PyBox {
                handle: Arc::new(cloned),
            })
        })
    }

    /// Copy from host into the box container rootfs.
    #[pyo3(signature = (host_path, container_dest, copy_options=None))]
    fn copy_in<'a>(
        &self,
        py: Python<'a>,
        host_path: String,
        container_dest: String,
        copy_options: Option<crate::options::PyCopyOptions>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let opts: boxlite::CopyOptions =
                copy_options.map_or_else(boxlite::CopyOptions::default, Into::into);

            handle
                .copy_into(std::path::Path::new(&host_path), &container_dest, opts)
                .await
                .map_err(map_err)?;
            Ok(())
        })
    }

    /// Copy from box container rootfs to host.
    #[pyo3(signature = (container_src, host_dest, copy_options=None))]
    fn copy_out<'a>(
        &self,
        py: Python<'a>,
        container_src: String,
        host_dest: String,
        copy_options: Option<crate::options::PyCopyOptions>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let opts: boxlite::CopyOptions =
                copy_options.map_or_else(boxlite::CopyOptions::default, Into::into);

            handle
                .copy_out(&container_src, std::path::Path::new(&host_dest), opts)
                .await
                .map_err(map_err)?;
            Ok(())
        })
    }

    /// Stream an archive into the box, without a file on the host.
    ///
    /// `source_is_dir` is the shape of the archive being written: `True` for a
    /// directory tree, `False` for a single file, and `None` when the caller
    /// cannot tell — the box then peeks the archive itself, which costs it a
    /// staged copy, so pass the shape whenever it is known.
    ///
    /// Returns a `CopyInStream`: write chunks, then `close()` (or use it as an
    /// async context manager, which closes on the way out and aborts if the
    /// body raised).
    #[pyo3(signature = (container_dest, source_is_dir=None, copy_options=None))]
    fn copy_in_stream(
        &self,
        container_dest: String,
        source_is_dir: Option<bool>,
        copy_options: Option<crate::options::PyCopyOptions>,
    ) -> PyResult<crate::copy_stream::PyCopyInStream> {
        let opts: boxlite::CopyOptions =
            copy_options.map_or_else(boxlite::CopyOptions::default, Into::into);
        Ok(crate::copy_stream::start_copy_in(
            Arc::clone(&self.handle),
            container_dest,
            boxlite::CopySourceKind::from_wire(source_is_dir),
            opts,
        ))
    }

    /// Read an archive out of the box as a stream of chunks, without a file on
    /// the host.
    ///
    /// Returns a `CopyOutStream` — async-iterable, and carrying the archive's
    /// shape on `source_is_dir`.
    #[pyo3(signature = (container_src, copy_options=None))]
    fn copy_out_stream<'a>(
        &self,
        py: Python<'a>,
        container_src: String,
        copy_options: Option<crate::options::PyCopyOptions>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&self.handle);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let opts: boxlite::CopyOptions =
                copy_options.map_or_else(boxlite::CopyOptions::default, Into::into);

            let (stream, source) = handle
                .copy_out_stream(&container_src, opts)
                .await
                .map_err(map_err)?;
            Ok(crate::copy_stream::PyCopyOutStream::new(stream, source))
        })
    }

    /// Enter async context manager - auto-starts the box (Testcontainers pattern).
    fn __aenter__<'a>(slf: PyRefMut<'_, Self>, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let handle = Arc::clone(&slf.handle);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            // Auto-start on context entry
            handle.start().await.map_err(map_err)?;
            Ok(PyBox { handle })
        })
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
