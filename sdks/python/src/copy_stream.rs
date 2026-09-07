//! Streaming file copy: move an archive in or out without a file on either
//! side.
//!
//! The path-based [`crate::box_handle::PyBox::copy_in`] pair is still the
//! answer for "copy this file into the box". These two are for bytes that have
//! no path — a payload built in memory, a download being relayed, a tar the
//! caller pipes through — and they mirror the shape the C and Go SDKs already
//! expose.

use std::sync::Arc;

use pyo3::types::PyBytes;
use pyo3::{Bound, Py, PyAny, PyRef, PyResult, Python, pyclass, pymethods};
use tokio::sync::{Mutex, mpsc};

use crate::util::map_err;

/// In-flight chunks a writer may run ahead of the upload.
///
/// The same window the C ABI's copy-in stream uses: bounded, so a producer
/// faster than the guest is slowed down rather than buffered without limit.
const WRITE_WINDOW: usize = 4;

/// The writer half of a copy-in: `None` once the archive has been ended.
type ChunkSender = Arc<Mutex<Option<mpsc::Sender<std::io::Result<Vec<u8>>>>>>;

/// The copy running behind a writer, joined by `close`/`abort`.
type UploadTask = Arc<Mutex<Option<tokio::task::JoinHandle<boxlite::BoxliteResult<()>>>>>;

/// Set by `close` to mean "the archive really ended here".
///
/// Dropping the writer drops the last `Sender`, which ends the channel as
/// *cleanly* as a `close` would — and a tar cut on a block boundary extracts
/// without complaint, so an abandoned copy would otherwise be reported as a
/// successful one. The consumer reads this once the channel is exhausted and
/// turns an unmarked end into a terminal error.
type ArchiveEnded = Arc<std::sync::atomic::AtomicBool>;

/// An archive being read out of a box, chunk by chunk.
///
/// Async-iterable: `async for chunk in stream`.
#[pyclass(name = "CopyOutStream")]
pub(crate) struct PyCopyOutStream {
    stream: Arc<Mutex<boxlite::BoxByteStream>>,
    source_is_dir: Option<bool>,
}

impl PyCopyOutStream {
    pub(crate) fn new(stream: boxlite::BoxByteStream, source: boxlite::CopySourceKind) -> Self {
        Self {
            stream: Arc::new(Mutex::new(stream)),
            source_is_dir: source.to_wire(),
        }
    }
}

#[pymethods]
impl PyCopyOutStream {
    /// Shape of what was archived: `True` for a directory tree, `False` for a
    /// single file, `None` when the box could not tell (an older guest), in
    /// which case the archive itself is the only source of truth.
    #[getter]
    fn source_is_dir(&self) -> Option<bool> {
        self.source_is_dir
    }

    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'a>(&self, py: Python<'a>) -> PyResult<Option<Bound<'a, PyAny>>> {
        let stream = Arc::clone(&self.stream);

        let future = pyo3_async_runtimes::tokio::future_into_py(py, async move {
            use futures::StreamExt;
            let mut guard = stream.lock().await;
            match guard.next().await {
                Some(Ok(chunk)) => Python::attach(|py| Ok(PyBytes::new(py, &chunk).unbind())),
                // A terminal error, not an end: raising here is what stops a
                // caller from treating a severed transfer as a whole archive.
                Some(Err(e)) => Err(pyo3::exceptions::PyIOError::new_err(e.to_string())),
                None => Err(pyo3::exceptions::PyStopAsyncIteration::new_err("")),
            }
        })?;

        Ok(Some(future))
    }

    fn __repr__(&self) -> String {
        format!("CopyOutStream(source_is_dir={:?})", self.source_is_dir)
    }
}

/// An archive being written into a box, chunk by chunk.
///
/// The upload runs while chunks are written, so a failure on either side —
/// the guest refusing the destination, or the caller aborting — surfaces from
/// [`Self::close`].
///
/// A writer abandoned without [`Self::close`] has its copy *reported as
/// failed*: only `close` marks the archive as ended, and an unmarked end
/// reaches the box as an error rather than an EOF it would extract happily.
/// Bytes the box already wrote can still be on its filesystem — that is true
/// of any mid-stream failure on the streamed path — so the guarantee is "no
/// silent success", not "nothing landed". `__aexit__` closes on the way out,
/// and aborts if the body raised.
#[pyclass(name = "CopyInStream")]
pub(crate) struct PyCopyInStream {
    chunks: ChunkSender,
    upload: UploadTask,
    ended: ArchiveEnded,
}

#[pymethods]
impl PyCopyInStream {
    /// Hand the next chunk to the upload, waiting if it is already
    /// [`WRITE_WINDOW`] chunks ahead.
    fn write<'a>(&self, py: Python<'a>, data: Vec<u8>) -> PyResult<Bound<'a, PyAny>> {
        let chunks = Arc::clone(&self.chunks);
        let upload = Arc::clone(&self.upload);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let sent = {
                let guard = chunks.lock().await;
                let Some(sender) = guard.as_ref() else {
                    return Err(pyo3::exceptions::PyValueError::new_err("write after close"));
                };
                sender.send(Ok(data)).await
            };
            if sent.is_ok() {
                return Ok(());
            }
            // The upload ended before this chunk, and its own error is the
            // real reason — a refused destination, a failed extraction. Raise
            // that instead of the send failure, which only says the channel is
            // gone. Without this the reason is lost for good: the caller
            // aborts on the write error and `abort` discards the copy's
            // result, so only archives small enough to fit the write window
            // ever reach `close`.
            join_upload(upload).await?;
            Err(pyo3::exceptions::PyIOError::new_err(
                "the copy ended before this chunk",
            ))
        })
    }

    /// Signal end of archive and wait for the box to finish extracting it.
    ///
    /// This is where a refused destination or a failed extraction is raised —
    /// the writes before it only queue bytes.
    fn close<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let chunks = Arc::clone(&self.chunks);
        let upload = Arc::clone(&self.upload);
        let ended = Arc::clone(&self.ended);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            // Marked *before* the sender goes, so the consumer cannot reach
            // the exhausted channel while the flag still says otherwise.
            ended.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(chunks.lock().await.take());
            join_upload(upload).await
        })
    }

    /// Fail the copy instead of finishing it.
    ///
    /// The box sees a terminal error rather than a clean end, so a truncated
    /// archive cannot be mistaken for a whole one. Returns normally — the
    /// caller is already handling whatever made them abort.
    fn abort<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let chunks = Arc::clone(&self.chunks);
        let upload = Arc::clone(&self.upload);

        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            if let Some(sender) = chunks.lock().await.take() {
                let _ = sender
                    .send(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "copy aborted by the caller",
                    )))
                    .await;
            }
            let _ = join_upload(upload).await;
            Ok(())
        })
    }

    fn __aenter__<'a>(slf: PyRef<'_, Self>, py: Python<'a>) -> PyResult<Bound<'a, PyAny>> {
        let handle: Py<Self> = slf.into();
        pyo3_async_runtimes::tokio::future_into_py(py, async move { Ok(handle) })
    }

    #[pyo3(signature = (exc_type=None, exc_value=None, traceback=None))]
    fn __aexit__<'a>(
        &self,
        py: Python<'a>,
        exc_type: Option<Bound<'a, PyAny>>,
        exc_value: Option<Bound<'a, PyAny>>,
        traceback: Option<Bound<'a, PyAny>>,
    ) -> PyResult<Bound<'a, PyAny>> {
        let _ = (exc_value, traceback);
        // An exception on the way out means the archive is incomplete, so the
        // copy must fail rather than commit what did arrive.
        if exc_type.is_some() {
            self.abort(py)
        } else {
            self.close(py)
        }
    }

    fn __repr__(&self) -> String {
        "CopyInStream(...)".to_string()
    }
}

/// Wait for the upload task, flattening "the task died" and "the copy failed".
async fn join_upload(upload: UploadTask) -> PyResult<()> {
    let Some(task) = upload.lock().await.take() else {
        // Already awaited: closing twice is not an error.
        return Ok(());
    };
    task.await
        .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("copy task failed: {e}")))?
        .map_err(map_err)
}

/// Start an upload and hand back the writer for it.
///
/// Lives here rather than on `PyBox` so the channel, the spawned upload and
/// the handle that feeds it are created in one place.
pub(crate) fn start_copy_in(
    handle: Arc<boxlite::LiteBox>,
    container_dest: String,
    source: boxlite::CopySourceKind,
    opts: boxlite::CopyOptions,
) -> PyCopyInStream {
    let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(WRITE_WINDOW);
    let ended: ArchiveEnded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marker = Arc::clone(&ended);
    let chunks = futures::stream::unfold(Some(rx), move |state| {
        let marker = Arc::clone(&marker);
        async move {
            let mut rx = state?;
            match rx.recv().await {
                Some(item) => Some((item, Some(rx))),
                // Channel exhausted. Only `close` marks the archive as ended;
                // anything else got here by dropping the writer, and must not
                // look like a whole archive.
                None if marker.load(std::sync::atomic::Ordering::SeqCst) => None,
                None => Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "copy stream dropped without close()",
                    )),
                    None,
                )),
            }
        }
    });
    let upload = pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
        handle
            .copy_in_stream(chunks, &container_dest, source, opts)
            .await
    });

    PyCopyInStream {
        chunks: Arc::new(Mutex::new(Some(tx))),
        upload: Arc::new(Mutex::new(Some(upload))),
        ended,
    }
}
