//! LiteBox - Individual box lifecycle management
//!
//! Provides lazy initialization and execution capabilities for isolated boxes.

pub(crate) mod archive;
mod attach;
pub(crate) mod box_impl;
mod clone_export;
pub(crate) mod config;
pub mod copy;
mod crash_report;
mod exec;
mod init;
pub(crate) mod local_snapshot;
mod manager;
mod network;
pub(crate) mod ports;
mod snapshot;
pub(crate) mod snapshot_mgr;
mod state;
mod watcher;

pub use attach::AttachOptions;
pub use copy::{CopyOptions, CopySourceKind};
pub(crate) use crash_report::CrashReport;
pub use exec::{BoxCommand, ExecResult, ExecStderr, ExecStdin, ExecStdout, Execution, ExecutionId};
pub(crate) use manager::BoxManager;
pub use network::{
    BoxConnection, BoxReader, BoxTunnel, BoxWriter, NetworkHandle, SocketAddress, TunnelForwarder,
};
pub use snapshot::SnapshotHandle;
pub use state::{BoxState, BoxStatus, HealthState, HealthStatus};

pub(crate) use box_impl::SharedBoxImpl;
pub(crate) use init::BoxBuilder;
pub(crate) use local_snapshot::LocalSnapshotBackend;

use std::path::Path;
use std::sync::Arc;

use crate::metrics::BoxMetrics;
use crate::runtime::backend::{BoxBackend, BoxNetworkBackend, SnapshotBackend};
use crate::runtime::options::{BoxArchive, CloneOptions, ExportOptions};
use crate::{BoxID, BoxInfo};
use boxlite_shared::errors::BoxliteResult;
pub use config::BoxConfig;

/// LiteBox - Handle to a box.
///
/// Thin wrapper delegating to a `BoxBackend` implementation.
/// Local backend: `BoxImpl` (VM-backed). REST backend: `RestBox` (HTTP-backed).
///
/// Following the same pattern as BoxliteRuntime wrapping RuntimeBackend.
pub struct LiteBox {
    /// Box ID for quick access without locking.
    id: BoxID,
    /// Box name for quick access without locking.
    name: Option<String>,
    /// Backend for lifecycle/exec/file operations.
    box_backend: Arc<dyn BoxBackend>,
    /// Backend for network operations.
    network_backend: Arc<dyn BoxNetworkBackend>,
    /// Backend for snapshot lifecycle operations.
    snapshot_backend: Arc<dyn SnapshotBackend>,
}

impl LiteBox {
    /// Create a LiteBox from backend implementations.
    pub(crate) fn new(
        box_backend: Arc<dyn BoxBackend>,
        network_backend: Arc<dyn BoxNetworkBackend>,
        snapshot_backend: Arc<dyn SnapshotBackend>,
    ) -> Self {
        let id = box_backend.id().clone();
        let name = box_backend.name().map(|s| s.to_string());
        Self {
            id,
            name,
            box_backend,
            network_backend,
            snapshot_backend,
        }
    }

    pub fn id(&self) -> &BoxID {
        &self.id
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Get metadata for this box.
    pub async fn info(&self) -> BoxliteResult<BoxInfo> {
        self.box_backend.info().await
    }

    /// Start the box (initialize VM).
    ///
    /// For Configured boxes: initializes VM for the first time.
    /// For Stopped boxes: restarts the VM.
    ///
    /// This is idempotent - calling start() on a Running box is a no-op.
    /// Also called implicitly by exec() if the box is not running.
    pub async fn start(&self) -> BoxliteResult<()> {
        self.box_backend.start().await
    }

    pub async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution> {
        self.box_backend.exec(command).await
    }

    /// Attach to a session in the box.
    ///
    /// [`AttachOptions::main`] follows the box's main command session (`run
    /// IMAGE COMMAND` runs COMMAND *as* the container init, docker semantics;
    /// the unqualified verb follows `docker attach` / `podman attach` / CRI
    /// `Attach`). This boots the box but does not run the command — call
    /// `start()` after. That is docker's create → attach → start: attach first,
    /// so a command that finishes instantly cannot outrun the stream and take
    /// its output and exit code with it.
    ///
    /// [`AttachOptions::execution`] reattaches to a running exec session by id
    /// (docker's `ContainerExecAttach`), returning a fresh `Execution` on a new
    /// stream; the caller discards any previous handle for the same id. Used
    /// after a transient WebSocket drop to resume stdio without restarting the
    /// process. `BoxliteError::SessionReaped` if it is no longer attachable.
    /// Only the REST backend models these; a local box supports `main()` only.
    ///
    /// [`AttachOptions::read_only`] attaches without stdin — docker's
    /// `--no-stdin`. The returned `Execution` has no stdin sender.
    pub async fn attach(&self, options: AttachOptions) -> BoxliteResult<Execution> {
        self.box_backend.attach(options).await
    }

    pub async fn metrics(&self) -> BoxliteResult<BoxMetrics> {
        self.box_backend.metrics().await
    }

    pub async fn stop(&self) -> BoxliteResult<()> {
        self.box_backend.stop().await
    }

    /// Copy files/directories from host into the container rootfs.
    pub async fn copy_into(
        &self,
        host_src: impl AsRef<Path>,
        container_dst: impl AsRef<str>,
        opts: copy::CopyOptions,
    ) -> BoxliteResult<()> {
        self.box_backend
            .copy_into(host_src.as_ref(), container_dst.as_ref(), opts)
            .await
    }

    /// Copy files/directories from container rootfs to host.
    pub async fn copy_out(
        &self,
        container_src: impl AsRef<str>,
        host_dst: impl AsRef<Path>,
        opts: copy::CopyOptions,
    ) -> BoxliteResult<()> {
        self.box_backend
            .copy_out(container_src.as_ref(), host_dst.as_ref(), opts)
            .await
    }

    /// Stream opaque transfer bytes into the container at `container_dst`.
    ///
    /// `source` is the archive shape (directory tree vs single file);
    /// [`CopySourceKind::Unknown`] when the caller cannot tell — the receiver
    /// then peeks the archive to decide. Every backend supports this.
    ///
    /// Returns a `BoxFuture` (rather than being an `async fn`) so the future
    /// owns everything it needs and borrows no part of `self` — the FFI
    /// backends spawn it onto the runtime.
    pub fn copy_in_stream<S>(
        &self,
        stream: S,
        container_dst: &str,
        source: copy::CopySourceKind,
        opts: copy::CopyOptions,
    ) -> futures::future::BoxFuture<'static, BoxliteResult<()>>
    where
        S: futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static,
    {
        Arc::clone(&self.box_backend).copy_in_stream(
            Box::pin(stream),
            container_dst.to_string(),
            source,
            opts,
        )
    }

    /// Download `container_src` as a byte stream plus its source shape.
    pub fn copy_out_stream(
        &self,
        container_src: &str,
        opts: copy::CopyOptions,
    ) -> futures::future::BoxFuture<
        'static,
        BoxliteResult<(boxlite_shared::BoxByteStream, copy::CopySourceKind)>,
    > {
        Arc::clone(&self.box_backend).copy_out_stream(container_src.to_string(), opts)
    }

    /// Get a network handle for raw tunnel operations.
    pub fn network(&self) -> NetworkHandle {
        NetworkHandle::new(Arc::clone(&self.network_backend))
    }

    /// Get a snapshot handle for snapshot operations.
    pub fn snapshots(&self) -> SnapshotHandle {
        SnapshotHandle::new(Arc::clone(&self.snapshot_backend))
    }

    /// Clone this box, creating a new box with a copy of its disks.
    pub async fn clone_box(
        &self,
        options: CloneOptions,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        self.box_backend.clone_box(options, name).await
    }

    /// Batch clone: create N clones sharing a single base disk copy.
    ///
    /// More efficient than calling `clone_box` N times: source disks are copied
    /// once into a shared base, then each clone gets a thin overlay (~64KB).
    pub async fn clone_boxes(
        &self,
        options: CloneOptions,
        count: usize,
        names: Vec<String>,
    ) -> BoxliteResult<Vec<LiteBox>> {
        self.box_backend.clone_boxes(options, count, names).await
    }

    /// Export this box as a portable `.boxlite` archive.
    pub async fn export(&self, options: ExportOptions, dest: &Path) -> BoxliteResult<BoxArchive> {
        self.box_backend.export_box(options, dest).await
    }
}

// ============================================================================
// THREAD SAFETY ASSERTIONS
// ============================================================================

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<LiteBox>;
};
