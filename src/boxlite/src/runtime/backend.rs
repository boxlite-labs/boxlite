//! Runtime backend trait — internal abstraction for local vs REST execution.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use boxlite_shared::BoxByteStream;
use futures::future::BoxFuture;

use crate::litebox::copy::{CopyOptions, CopySourceKind};
use crate::litebox::snapshot_mgr::SnapshotInfo;
use crate::litebox::{AttachOptions, BoxCommand, BoxTunnel, Execution, LiteBox};
use crate::metrics::{BoxMetrics, RuntimeMetrics};
use crate::runtime::options::{
    BoxArchive, BoxOptions, CloneOptions, ExportOptions, SnapshotOptions,
};
use crate::runtime::types::BoxInfo;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::id::BoxID;

/// Backend abstraction for runtime operations.
///
/// Local backend delegates to `RuntimeImpl` (VM management).
/// REST backend delegates to HTTP API calls.
///
/// This trait is `pub(crate)` — internal implementation detail.
/// The public API (`BoxliteRuntime`) is unchanged.
#[async_trait]
pub(crate) trait RuntimeBackend: Send + Sync {
    async fn create(&self, options: BoxOptions, name: Option<String>) -> BoxliteResult<LiteBox>;

    async fn get_or_create(
        &self,
        options: BoxOptions,
        name: Option<String>,
    ) -> BoxliteResult<(LiteBox, bool)>;

    async fn get(&self, id_or_name: &str) -> BoxliteResult<Option<LiteBox>>;

    async fn get_info(&self, id_or_name: &str) -> BoxliteResult<Option<BoxInfo>>;

    async fn list_info(&self) -> BoxliteResult<Vec<BoxInfo>>;

    async fn exists(&self, id_or_name: &str) -> BoxliteResult<bool>;

    async fn metrics(&self) -> BoxliteResult<RuntimeMetrics>;

    async fn remove(&self, id_or_name: &str, force: bool) -> BoxliteResult<()>;

    async fn shutdown(&self, timeout: Option<i32>) -> BoxliteResult<()>;

    async fn import_box(
        &self,
        _archive: BoxArchive,
        _name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        Err(BoxliteError::Unsupported(
            "This operation is only supported for local runtimes (not REST backends)".to_string(),
        ))
    }

    /// Synchronous shutdown for atexit/Drop contexts.
    /// Default no-op (REST backend doesn't manage local processes).
    fn shutdown_sync(&self) {}
}

/// Backend abstraction for individual box operations.
///
/// Local backend is implemented directly by `BoxImpl`.
/// REST backend delegates to HTTP API calls.
#[async_trait]
pub(crate) trait BoxBackend: Send + Sync {
    fn id(&self) -> &BoxID;

    fn name(&self) -> Option<&str>;

    /// Return metadata for this box.
    async fn info(&self) -> BoxliteResult<BoxInfo>;

    async fn start(&self) -> BoxliteResult<()>;

    async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution>;

    /// Attach to a session in the box.
    ///
    /// [`AttachOptions::main`] follows the box's main command session (docker
    /// semantics: `run`'s COMMAND is the container init, registered under
    /// execution_id = container id, which the caller cannot name). The
    /// unqualified verb mirrors the ecosystem convention (`ContainerAttach`,
    /// `podman attach`, CRI `Attach`).
    ///
    /// [`AttachOptions::execution`] reattaches to an already-running exec
    /// session (docker's `ContainerExecAttach`): a fresh `Execution` on a new
    /// stream, so the caller discards any prior handle for the same id. Returns
    /// `BoxliteError::SessionReaped` if the server reports it no longer
    /// attachable.
    ///
    /// [`AttachOptions::read_only`] drops stdin. Backends must enforce that
    /// rather than rely on the caller: the returned `Execution` has no stdin
    /// sender, and a backend that talks over a bidirectional transport refuses
    /// writes on it.
    ///
    /// Default is `Unsupported`; a backend implements the arms it models. A local
    /// in-process backend has no long-lived reattachable exec sessions, so it
    /// supports `main()` only.
    async fn attach(&self, _options: AttachOptions) -> BoxliteResult<Execution> {
        Err(BoxliteError::Unsupported(
            "this backend does not support attaching to sessions".into(),
        ))
    }

    async fn metrics(&self) -> BoxliteResult<BoxMetrics>;

    async fn stop(&self) -> BoxliteResult<()>;

    async fn copy_into(
        &self,
        host_src: &Path,
        container_dst: &str,
        opts: CopyOptions,
    ) -> BoxliteResult<()>;

    async fn copy_out(
        &self,
        container_src: &str,
        host_dst: &Path,
        opts: CopyOptions,
    ) -> BoxliteResult<()>;

    /// Stream opaque transfer bytes into the box at `container_dst`.
    ///
    /// Plain `fn` returning a `'static` future rather than an `async fn`: the
    /// FFI backends spawn a copy onto the runtime, which needs a future that
    /// borrows nothing. `Arc<Self>` is a dyn-dispatchable receiver, and the
    /// erased [`BoxByteStream`] (not a generic `S: Stream`) is what keeps the
    /// method object-safe — [`crate::LiteBox`] boxes once on the way in.
    fn copy_in_stream(
        self: Arc<Self>,
        stream: BoxByteStream,
        container_dst: String,
        source: CopySourceKind,
        opts: CopyOptions,
    ) -> BoxFuture<'static, BoxliteResult<()>>;

    /// Read `container_src` out as a byte stream, plus the shape the peer
    /// reported for it ([`CopySourceKind::Unknown`] when it could not tell).
    fn copy_out_stream(
        self: Arc<Self>,
        container_src: String,
        opts: CopyOptions,
    ) -> BoxFuture<'static, BoxliteResult<(BoxByteStream, CopySourceKind)>>;

    async fn clone_box(
        &self,
        options: CloneOptions,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox>;

    async fn clone_boxes(
        &self,
        options: CloneOptions,
        count: usize,
        names: Vec<String>,
    ) -> BoxliteResult<Vec<LiteBox>>;

    async fn export_box(&self, options: ExportOptions, dest: &Path) -> BoxliteResult<BoxArchive>;
}

/// Backend abstraction for box network operations.
///
/// Kept separate from `BoxBackend` so lifecycle/exec/file operations do not own
/// network data-plane capabilities directly.
#[async_trait]
pub(crate) trait BoxNetworkBackend: Send + Sync {
    /// Establish a one-shot tunnel to a service port inside the box.
    async fn tunnel(&self, target: SocketAddr) -> BoxliteResult<BoxTunnel>;
}

/// Backend abstraction for snapshot lifecycle operations on a box.
///
/// Kept separate from `BoxBackend` so lifecycle/exec/file operations can evolve
/// independently from snapshot/clone/export behavior.
#[allow(dead_code)] // Snapshots temporarily disabled; will be re-enabled
#[async_trait]
pub(crate) trait SnapshotBackend: Send + Sync {
    async fn create(&self, options: SnapshotOptions, name: &str) -> BoxliteResult<SnapshotInfo>;

    async fn list(&self) -> BoxliteResult<Vec<SnapshotInfo>>;

    async fn get(&self, name: &str) -> BoxliteResult<Option<SnapshotInfo>>;

    async fn remove(&self, name: &str) -> BoxliteResult<()>;

    async fn restore(&self, name: &str) -> BoxliteResult<()>;
}

/// Backend abstraction for execution control (signal, kill, resize).
///
/// Local backend is implemented by `ExecutionInterface`. REST backend
/// delegates to HTTP API calls.
///
/// `signal(id, n)` sends signal `n` to the execution and returns; the exec
/// continues running (or not) based on whether the process honors the signal.
/// `kill(id)` is the explicit terminate-and-evict verb — for the REST
/// backend it issues `DELETE /executions/{id}`; for the local backend it
/// defaults to `signal(id, SIGKILL)`. Splitting them lets callers ask for
/// SIGINT/SIGTERM/SIGHUP without the request body being silently coerced
/// to SIGKILL on the server side (the historical bug).
#[async_trait]
pub(crate) trait ExecBackend: Send + Sync {
    async fn signal(&mut self, execution_id: &str, signal: i32) -> BoxliteResult<()>;

    async fn kill(&mut self, execution_id: &str) -> BoxliteResult<()> {
        self.signal(execution_id, 9).await
    }

    async fn resize_tty(
        &mut self,
        execution_id: &str,
        rows: u32,
        cols: u32,
        x_pixels: u32,
        y_pixels: u32,
    ) -> BoxliteResult<()>;
}
