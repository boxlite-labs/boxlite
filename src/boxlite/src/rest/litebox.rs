//! RestBox — implements BoxBackend for the REST API.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use reqwest::Method;
use tokio::sync::mpsc;

use boxlite_shared::BoxByteStream;
use boxlite_shared::constants::files::FALLBACK_CAP_BYTES;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use futures::StreamExt;
use std::io;

use crate::BoxInfo;
use crate::litebox::copy::{CopyOptions, CopySourceKind};
use crate::litebox::snapshot_mgr::SnapshotInfo;
use crate::litebox::{
    AttachOptions, BoxCommand, BoxTunnel, ExecResult, ExecStderr, ExecStdin, ExecStdout, Execution,
};
use crate::metrics::BoxMetrics;
use crate::runtime::backend::{BoxBackend, BoxNetworkBackend, SnapshotBackend};
use crate::runtime::id::BoxID;
use crate::runtime::options::{CloneOptions, ExportOptions, SnapshotOptions};

use super::client::{ApiClient, WsStream, transport_error};
use super::error::map_http_body;
use super::exec::RestExecControl;
use super::types::{
    BoxMetricsResponse, BoxResponse, CloneBoxRequest, CreateSnapshotRequest, ExecRequest,
    ExecResponse, ExecutionStatusResponse, ExportBoxRequest, ListSnapshotsResponse,
    SnapshotResponse,
};

/// REST-backed box handle.
///
/// Holds immutable identity for synchronous accessors and delegates operations,
/// including `info()`, to the remote REST API.
pub(crate) struct RestBox {
    client: ApiClient,
    id: BoxID,
    name: Option<String>,
}

impl RestBox {
    pub fn new(client: ApiClient, info: BoxInfo) -> Self {
        Self {
            client,
            id: info.id.clone(),
            name: info.name.clone(),
        }
    }

    fn box_id_str(&self) -> String {
        self.id.to_string()
    }

    fn validate_info(&self, info: BoxInfo) -> BoxliteResult<BoxInfo> {
        if info.id != self.id {
            return Err(BoxliteError::Internal(format!(
                "REST response box_id {} does not match handle {}",
                info.id, self.id
            )));
        }
        Ok(info)
    }

    /// Wire an already-open attach WebSocket into an `Execution`.
    ///
    /// Shared by both arms of [`BoxBackend::attach`] — the box's main command
    /// session (`None`) and a tenant exec (`Some(id)`): once the socket is
    /// open, the two are the same session — same pump, same channels, same
    /// control routes (`RestExecControl` addresses both by execution id).
    /// Callers open the socket themselves, and do it synchronously, so a
    /// 404/409 surfaces at *their* await point rather than arriving later
    /// as a synthesized `ExecResult`.
    fn wire_attach(
        &self,
        box_id: String,
        execution_id: String,
        stream: WsStream,
        wire_stdin: bool,
    ) -> Execution {
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel::<String>();
        let (result_tx, result_rx) = mpsc::unbounded_channel::<ExecResult>();
        let stdin = wire_stdin.then(mpsc::unbounded_channel::<Vec<u8>>);
        let (stdin_tx, stdin_rx) = match stdin {
            Some((tx, rx)) => (Some(tx), Some(rx)),
            None => (None, None),
        };

        let ws_client = self.client.clone();
        let ws_box_id = box_id.clone();
        let ws_exec_id = execution_id.clone();
        tokio::spawn(async move {
            attach_ws_pump(
                &ws_client,
                &ws_box_id,
                &ws_exec_id,
                stream,
                stdin_rx,
                stdout_tx,
                stderr_tx,
                result_tx,
            )
            .await;
        });

        let control = RestExecControl::new(self.client.clone(), box_id);
        Execution::new(
            execution_id,
            Box::new(control),
            result_rx,
            stdin_tx.map(ExecStdin::new),
            Some(ExecStdout::new(stdout_rx)),
            Some(ExecStderr::new(stderr_rx)),
        )
    }
}

impl RestBox {
    /// PUT an archive stream at `container_dst`, carrying the shape hint.
    ///
    /// The one place the upload wire format lives, so `copy_into` (which packs
    /// a host path first) and `copy_in_stream` (which is handed the bytes)
    /// cannot drift apart.
    async fn upload_archive(
        &self,
        archive: BoxByteStream,
        container_dst: &str,
        source: CopySourceKind,
    ) -> BoxliteResult<()> {
        let body = reqwest::Body::wrap_stream(archive.map(|r| r.map(bytes::Bytes::from)));

        let encoded_dst = urlencoding::encode(container_dst);
        // The hint is omitted, not guessed, when the caller could not tell:
        // the server then peeks the archive, its pre-hint behaviour.
        let hint = match source.to_wire() {
            Some(is_dir) => format!("&source_is_dir={is_dir}"),
            None => String::new(),
        };
        let path = format!(
            "/boxes/{}/files?path={}{}",
            self.box_id_str(),
            encoded_dst,
            hint
        );
        let builder = self
            .client
            .authorized_request(Method::PUT, &path)
            .await?
            .header("Content-Type", "application/x-tar")
            .body(body);

        let resp = builder.send().await.map_err(transport_error)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(map_http_body(status, &text));
        }
        Ok(())
    }

    /// GET `container_src` as an archive, returning the live response and the
    /// shape header the server sent (`None` from a server predating it).
    ///
    /// Hands back the `Response` rather than a byte stream because the two
    /// callers need different things from it: `copy_out` may still have to
    /// buffer it (and wants `reqwest`'s error classification while doing so),
    /// while `copy_out_stream` only forwards the bytes.
    async fn open_archive(
        &self,
        container_src: &str,
    ) -> BoxliteResult<(reqwest::Response, Option<bool>)> {
        let encoded_src = urlencoding::encode(container_src);
        let path = format!("/boxes/{}/files?path={}", self.box_id_str(), encoded_src);
        let builder = self
            .client
            .authorized_request(Method::GET, &path)
            .await?
            .header("Accept", "application/x-tar");

        let resp = builder.send().await.map_err(transport_error)?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(map_http_body(status, &text));
        }

        let source_is_dir = resp
            .headers()
            .get("x-boxlite-source-is-dir")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<bool>().ok());
        Ok((resp, source_is_dir))
    }
}

#[async_trait]
impl BoxBackend for RestBox {
    fn id(&self) -> &BoxID {
        &self.id
    }

    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    async fn info(&self) -> BoxliteResult<BoxInfo> {
        let box_id = self.box_id_str();
        let path = format!("/boxes/{box_id}");
        let response: BoxResponse = self.client.get(&path).await?;
        let info = response.to_box_info()?;
        self.validate_info(info)
    }

    async fn start(&self) -> BoxliteResult<()> {
        let box_id = self.box_id_str();
        let path = format!("/boxes/{}/start", box_id);
        let resp: BoxResponse = self.client.post_empty(&path).await?;
        self.validate_info(resp.to_box_info()?)?;
        Ok(())
    }

    async fn exec(&self, command: BoxCommand) -> BoxliteResult<Execution> {
        let box_id = self.box_id_str();

        // 1. Create execution on remote server
        let path = format!("/boxes/{}/exec", box_id);
        let req = ExecRequest::from_command(&command);
        let resp: ExecResponse = self.client.post(&path, &req).await?;
        let execution_id = resp.execution_id;

        // 2. Set up channels for stdout, stderr, stdin, and result
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, stderr_rx) = mpsc::unbounded_channel::<String>();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (result_tx, result_rx) = mpsc::unbounded_channel::<ExecResult>();

        // 3. Spawn the bidirectional WebSocket pump (stdin + stdout + stderr + exit)
        let ws_client = self.client.clone();
        let ws_box_id = box_id.clone();
        let ws_exec_id = execution_id.clone();
        tokio::spawn(async move {
            attach_ws(
                &ws_client,
                &ws_box_id,
                &ws_exec_id,
                stdin_rx,
                stdout_tx,
                stderr_tx,
                result_tx,
            )
            .await;
        });

        // 4. Build Execution handle
        let control = RestExecControl::new(self.client.clone(), box_id);
        let stdout = ExecStdout::new(stdout_rx);
        let stderr = ExecStderr::new(stderr_rx);
        let stdin = ExecStdin::new(stdin_tx);

        Ok(Execution::new(
            execution_id,
            Box::new(control),
            result_rx,
            Some(stdin),
            Some(stdout),
            Some(stderr),
        ))
    }

    /// Attach to a session in the box.
    ///
    /// - `None` — the main command session (the container's init). Hits the
    ///   container-attach route (docker's `POST /containers/{id}/attach`), a
    ///   distinct resource from exec-attach: the client cannot name the init
    ///   session (its execution id is the container id, which `BoxInfo` does not
    ///   carry), so the server returns it on the upgrade header. The route boots
    ///   the box and subscribes to init's session but does not run it —
    ///   `POST /start` does. `run --url` calls `attach(None)` then `start()`, the
    ///   same create → attach → start it does locally, so a command that finishes
    ///   instantly cannot outrun the stream.
    /// - `Some(id)` — reattach to an existing exec session on
    ///   `/executions/{id}/attach`.
    ///
    /// Either way the returned `Execution` is a fully ordinary one — signal,
    /// resize and kill go out over the same `/executions/{id}/…` routes, and the
    /// pump reconnects through them too.
    async fn attach(&self, options: AttachOptions) -> BoxliteResult<Execution> {
        let box_id = self.box_id_str();

        // Open the WebSocket synchronously so a rejection (404 no-such-box /
        // reaped, 409 another client already attached) surfaces here, at the
        // caller's `await box.attach(..)`, not in a later ExecResult from `wait()`.
        // Read-only is asked for on the wire, not just honored locally: the
        // socket is bidirectional, so the server has to be the one refusing
        // writes.
        let query = if options.wants_stdin() {
            ""
        } else {
            "?stdin=0"
        };

        match options.execution_id() {
            None => {
                let path = format!("/boxes/{}/attach{}", box_id, query);
                let (stream, handshake) = self
                    .client
                    .connect_ws_with_response(&path)
                    .await
                    .map_err(|e| match e {
                        BoxliteError::AlreadyExists(msg) => BoxliteError::AlreadyExists(format!(
                            "box {} main session has another client attached: {}",
                            box_id, msg
                        )),
                        other => other,
                    })?;

                let execution_id = handshake
                    .headers()
                    .get(MAIN_SESSION_ID_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        BoxliteError::Unsupported(format!(
                            "server did not identify box {}'s main session (no {} header on the \
                             attach upgrade); the server predates container attach",
                            box_id, MAIN_SESSION_ID_HEADER
                        ))
                    })?
                    .to_string();

                Ok(self.wire_attach(box_id, execution_id, stream, options.wants_stdin()))
            }
            Some(execution_id) => {
                let path = format!(
                    "/boxes/{}/executions/{}/attach{}",
                    box_id, execution_id, query
                );
                let stream = self.client.connect_ws(&path).await.map_err(|e| match e {
                    BoxliteError::NotFound(msg) => BoxliteError::SessionReaped(format!(
                        "session {} not found — likely reaped after disconnect timeout: {}",
                        execution_id, msg
                    )),
                    BoxliteError::AlreadyExists(msg) => BoxliteError::AlreadyExists(format!(
                        "session {} has another client attached: {}",
                        execution_id, msg
                    )),
                    other => other,
                })?;

                Ok(self.wire_attach(
                    box_id,
                    execution_id.to_string(),
                    stream,
                    options.wants_stdin(),
                ))
            }
        }
    }

    async fn metrics(&self) -> BoxliteResult<BoxMetrics> {
        let box_id = self.box_id_str();
        let path = format!("/boxes/{}/metrics", box_id);
        let resp: BoxMetricsResponse = self.client.get(&path).await?;
        Ok(box_metrics_from_response(&resp))
    }

    async fn stop(&self) -> BoxliteResult<()> {
        let box_id = self.box_id_str();
        let path = format!("/boxes/{}/stop", box_id);
        let resp: BoxResponse = self.client.post_empty(&path).await?;
        self.validate_info(resp.to_box_info()?)?;
        Ok(())
    }

    async fn copy_into(
        &self,
        host_src: &Path,
        container_dst: &str,
        opts: CopyOptions,
    ) -> BoxliteResult<()> {
        // Honor overwrite=false at the REST boundary. The runner's
        // upload handler always extracts the tar over whatever's at
        // container_dst (the test in apps/e2e/cases/test_files_io.py::
        // test_copy_in_overwrite_false_rejects_conflict was catching
        // 'overwrite=False replaced guest content'). The test contract
        // explicitly accepts a raised exception OR a no-op — the
        // invariant is "guest content must be unchanged". We can't
        // express the per-entry "skip if dst exists" semantics over
        // the current single-tar upload protocol, so refuse the call
        // outright instead of silently clobbering.
        if !opts.overwrite {
            return Err(BoxliteError::Unsupported(
                "copy_into with overwrite=false is not supported over the REST backend; \
                 the current upload protocol cannot express per-entry skip-if-exists. \
                 Use the FFI backend or pre-check the destination before calling."
                    .into(),
            ));
        }

        // Stream a tar of the host source straight into the request body.
        let (source_is_dir, tar) = boxlite_shared::tar::pack_stream(
            host_src.to_path_buf(),
            boxlite_shared::tar::PackContext {
                follow_symlinks: opts.follow_symlinks,
                include_parent: opts.include_parent,
            },
        )
        .await?;

        self.upload_archive(
            Box::pin(tar),
            container_dst,
            CopySourceKind::from_wire(Some(source_is_dir)),
        )
        .await
    }

    async fn copy_out(
        &self,
        container_src: &str,
        host_dst: &Path,
        _opts: CopyOptions,
    ) -> BoxliteResult<()> {
        // The archive shape is carried on the response header (newer servers);
        // absence means the peer predates it → capped buffered fallback.
        let (resp, source_is_dir) = self.open_archive(container_src).await?;

        let Some(source_is_dir) = source_is_dir else {
            // Legacy peer: without the shape hint we must buffer to detect it.
            let bytes = read_capped(resp, FALLBACK_CAP_BYTES).await?;
            return extract_tar_to_path(&bytes, host_dst);
        };
        // The header carries the *source* shape; the destination's own
        // signals — an existing directory, or a trailing slash naming one —
        // must still win for a single file, matching the legacy
        // `detect_extraction_mode` and Unix cp semantics.
        let dest_wants_dir = host_dst.as_os_str().to_string_lossy().ends_with('/');
        let force_directory = source_is_dir || host_dst.is_dir() || dest_wants_dir;

        // A severed body must stay classified as a transport fault so callers
        // retry instead of filing a bug. `unpack_stream` flattens the byte
        // stream's `io::Error` into its own storage message, so park the
        // classified error in a shared slot and prefer it if the unpack then
        // fails. A `Mutex` (not `OnceLock` + `Arc::try_unwrap`) because the
        // forwarding task inside `unpack_stream` may still hold a clone of the
        // slot when we read it back — we must not depend on Arc drop timing.
        let transport_fault: Arc<Mutex<Option<BoxliteError>>> = Arc::new(Mutex::new(None));
        let fault_slot = Arc::clone(&transport_fault);
        let stream: boxlite_shared::BoxByteStream = Box::pin(resp.bytes_stream().map(move |r| {
            r.map(|b| b.to_vec()).map_err(|e| {
                let classified = transport_error(e);
                let message = classified.to_string();
                if let Ok(mut slot) = fault_slot.lock() {
                    *slot = Some(classified);
                }
                io::Error::other(message)
            })
        }));

        let unpacked = boxlite_shared::tar::unpack_stream(
            stream,
            host_dst.to_path_buf(),
            boxlite_shared::tar::UnpackContext {
                overwrite: true,
                mkdir_parents: true,
                force_directory,
            },
        )
        .await;

        match unpacked {
            Ok(_) => Ok(()),
            Err(unpack_err) => {
                let fault = transport_fault.lock().ok().and_then(|mut s| s.take());
                Err(fault.unwrap_or(unpack_err))
            }
        }
    }

    fn copy_in_stream(
        self: Arc<Self>,
        stream: BoxByteStream,
        container_dst: String,
        source: CopySourceKind,
        opts: CopyOptions,
    ) -> futures::future::BoxFuture<'static, BoxliteResult<()>> {
        Box::pin(async move {
            // Same protocol limit `copy_into` refuses on, and for the same
            // reason: a single-tar upload cannot express per-entry
            // skip-if-exists, so honouring `overwrite=false` here would mean
            // silently clobbering.
            if !opts.overwrite {
                return Err(BoxliteError::Unsupported(
                    "copy_in_stream with overwrite=false is not supported over the REST \
                     backend; the current upload protocol cannot express per-entry \
                     skip-if-exists. Use the FFI backend or pre-check the destination."
                        .into(),
                ));
            }
            self.upload_archive(stream, &container_dst, source).await
        })
    }

    fn copy_out_stream(
        self: Arc<Self>,
        container_src: String,
        _opts: CopyOptions,
    ) -> futures::future::BoxFuture<'static, BoxliteResult<(BoxByteStream, CopySourceKind)>> {
        Box::pin(async move {
            let (resp, source_is_dir) = self.open_archive(&container_src).await?;
            // No buffered fallback here, unlike `copy_out`: there is no
            // destination path to spool the archive to, so a server that omits
            // the header honestly reports `Unknown` and the caller — which
            // does have somewhere to put it — decides what to do.
            let stream: BoxByteStream = Box::pin(resp.bytes_stream().map(|chunk| {
                chunk.map(|b| b.to_vec()).map_err(|e| {
                    // Keep the transport classification in the message; the
                    // stream contract can only carry an `io::Error`.
                    std::io::Error::other(transport_error(e).to_string())
                })
            }));
            Ok((stream, CopySourceKind::from_wire(source_is_dir)))
        })
    }

    async fn clone_box(
        &self,
        options: CloneOptions,
        name: Option<String>,
    ) -> BoxliteResult<crate::LiteBox> {
        self.client.require_clone_enabled().await?;

        let box_id = self.box_id_str();
        let path = format!("/boxes/{}/clone", box_id);
        let req = CloneBoxRequest::from_options(&options, name.as_deref());
        let resp: BoxResponse = self.client.post(&path, &req).await?;

        let info = resp.to_box_info()?;
        let rest_box = Arc::new(RestBox::new(self.client.clone(), info));
        let box_backend: Arc<dyn BoxBackend> = rest_box.clone();
        let network_backend: Arc<dyn BoxNetworkBackend> = rest_box.clone();
        let snapshot_backend: Arc<dyn SnapshotBackend> = rest_box;
        Ok(crate::LiteBox::new(
            box_backend,
            network_backend,
            snapshot_backend,
        ))
    }

    async fn clone_boxes(
        &self,
        options: CloneOptions,
        count: usize,
        names: Vec<String>,
    ) -> BoxliteResult<Vec<crate::LiteBox>> {
        let mut results = Vec::with_capacity(count);
        for i in 0..count {
            let name = names.get(i).cloned();
            let litebox = self.clone_box(options.clone(), name).await?;
            results.push(litebox);
        }
        Ok(results)
    }

    async fn export_box(
        &self,
        options: ExportOptions,
        dest: &Path,
    ) -> BoxliteResult<crate::runtime::options::BoxArchive> {
        self.client.require_export_enabled().await?;

        let box_id = self.box_id_str();
        let path = format!("/boxes/{}/export", box_id);
        let req = ExportBoxRequest::from_options(&options);
        let archive_bytes = self.client.post_for_bytes(&path, &req).await?;

        let output_path = if dest.is_dir() {
            let name = self.name().unwrap_or("box");
            dest.join(format!("{}.boxlite", name))
        } else {
            dest.to_path_buf()
        };

        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create export destination directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        std::fs::write(&output_path, &archive_bytes).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to write export archive {}: {}",
                output_path.display(),
                e
            ))
        })?;

        Ok(crate::runtime::options::BoxArchive::new(output_path))
    }
}

#[async_trait]
impl BoxNetworkBackend for RestBox {
    async fn tunnel(&self, target: SocketAddr) -> BoxliteResult<BoxTunnel> {
        if target.ip().to_string() != crate::net::constants::GUEST_IP {
            return Err(BoxliteError::Unsupported(
                "REST box tunnels only support service ports on the guest IP".into(),
            ));
        }

        let port = target.port();
        let box_id = self.box_id_str();
        let endpoint = self.client.prepare_box_tunnel(&box_id, port).await?;
        let connection = self.client.connect_box_network_tunnel(&endpoint).await?;
        Ok(BoxTunnel::remote(endpoint, connection))
    }
}

#[async_trait]
impl SnapshotBackend for RestBox {
    async fn create(&self, options: SnapshotOptions, name: &str) -> BoxliteResult<SnapshotInfo> {
        self.client.require_snapshots_enabled().await?;

        let box_id = self.box_id_str();
        let path = format!("/boxes/{}/snapshots", box_id);
        let req = CreateSnapshotRequest::from_options(&options, name);
        let resp: SnapshotResponse = self.client.post(&path, &req).await?;
        Ok(resp.to_snapshot_info())
    }

    async fn list(&self) -> BoxliteResult<Vec<SnapshotInfo>> {
        self.client.require_snapshots_enabled().await?;

        let box_id = self.box_id_str();
        let path = format!("/boxes/{}/snapshots", box_id);
        let resp: ListSnapshotsResponse = self.client.get(&path).await?;
        Ok(resp
            .snapshots
            .iter()
            .map(SnapshotResponse::to_snapshot_info)
            .collect())
    }

    async fn get(&self, name: &str) -> BoxliteResult<Option<SnapshotInfo>> {
        self.client.require_snapshots_enabled().await?;

        let box_id = self.box_id_str();
        let encoded_name = urlencoding::encode(name);
        let path = format!("/boxes/{}/snapshots/{}", box_id, encoded_name);
        match self.client.get::<SnapshotResponse>(&path).await {
            Ok(resp) => Ok(Some(resp.to_snapshot_info())),
            Err(BoxliteError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn remove(&self, name: &str) -> BoxliteResult<()> {
        self.client.require_snapshots_enabled().await?;

        let box_id = self.box_id_str();
        let encoded_name = urlencoding::encode(name);
        let path = format!("/boxes/{}/snapshots/{}", box_id, encoded_name);
        self.client.delete(&path).await
    }

    async fn restore(&self, name: &str) -> BoxliteResult<()> {
        self.client.require_snapshots_enabled().await?;

        let box_id = self.box_id_str();
        let encoded_name = urlencoding::encode(name);
        let path = format!("/boxes/{}/snapshots/{}/restore", box_id, encoded_name);
        self.client.post_empty_no_content(&path).await
    }
}

// ============================================================================
// WebSocket Attach
// ============================================================================
//
// One bidirectional WebSocket carries stdin (Binary frames), stdout/stderr
// with a 1-byte channel prefix (0x01 / 0x02), and control messages (text
// JSON: resize / signal / stdin_eof / exit / error). Wire format is the
// authoritative one defined by the server attach handler — see plan D1/D2.

/// Upgrade-response header carrying the main session's execution id (the
/// container id) on `/boxes/{id}/attach`. Server counterpart:
/// `MAIN_SESSION_ID_HEADER` in `src/cli/src/commands/serve/mod.rs`. Like
/// the channel prefixes and control frames above, the attach wire contract
/// is duplicated on each side of the boundary rather than shared through a
/// crate.
const MAIN_SESSION_ID_HEADER: &str = "x-boxlite-execution-id";

/// Maximum idle interval before the WS reader gives up on the connection.
///
/// The watchdog catches silent CDN/proxy cuts that would otherwise leave the
/// reader parked forever on `stream.next().await`. Tests override this via
/// `cfg(test)` so they don't have to wait 45 s to observe a timeout.
#[cfg(not(test))]
const WS_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(45);
#[cfg(test)]
const WS_WATCHDOG: std::time::Duration = std::time::Duration::from_millis(300);

/// Time to wait for the *first* server frame after the WS upgrade
/// completes. A freshly-attached exec that produces no frame in this
/// window is almost certainly dead (missing box/exec, server upgraded
/// the socket but has nothing to stream, or a transport that tunnels
/// the HTTP upgrade but not WS data frames — e.g. an HTTP proxy). Using
/// the full steady-state `WS_WATCHDOG` here meant such cases burned the
/// entire reconnect budget (~minutes) before failing; this short bound
/// fails them fast. Once any server frame arrives the steady-state
/// `WS_WATCHDOG` governs idle detection as before.
#[cfg(not(test))]
const WS_FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const WS_FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// Total wall-clock budget for reconnecting after a transient WS disconnect.
///
/// Aligned with the runner's `defaultReconnectGrace = 5 minutes` (Phase 4 reaper
/// in `exec_manager.go`) — we want to reattach before the runner SIGHUPs the
/// orphaned exec. Tests use a much shorter budget to keep them fast.
#[cfg(not(test))]
const WS_RECONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(270);
#[cfg(test)]
const WS_RECONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(1);

// Initial backoff is intentionally larger than the runner's WS keepalive
// interval (15s in apps/runner/.../boxlite_exec_attach.go). The most common
// reattach failure is the old server-side `runAttachLoop` not yet having
// observed our TCP RST — `MarkConnected` then returns 409 until the server's
// own keepalive Ping write fails and cleanup runs. Sleeping past that
// interval avoids burning the reconnect budget on guaranteed-409 retries.
const WS_RECONNECT_BACKOFF_INITIAL: std::time::Duration = std::time::Duration::from_secs(15);
const WS_RECONNECT_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(30);

/// How often the client sends its own WebSocket Ping on an established,
/// otherwise-idle session.
///
/// The runner already pings client-ward every 15s, but those server->client
/// pings can be silently dropped or coalesced by an intermediary (ALB / CDN /
/// corporate proxy) that still tunnels the upgrade. With no client->server
/// traffic of its own, an idle interactive exec then looks dead to such an
/// intermediary, and the client's `WS_WATCHDOG` trips on a connection that is
/// actually fine. A client-initiated Ping guarantees bidirectional traffic and
/// feeds our own watchdog via the returned Pong, independent of whether the
/// server's pings survive every hop. Must stay comfortably below `WS_WATCHDOG`.
#[cfg(not(test))]
const WS_CLIENT_PING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
#[cfg(test)]
const WS_CLIENT_PING_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Drive the bidirectional WS attach for a single execution.
///
/// Wire contract (mirrors the server's `/executions/{id}/attach` handler):
///
/// - Client → Server: binary frames are stdin bytes; text JSON frames are
///   control (`resize`, `signal`, `stdin_eof`).
/// - Server → Client: binary frames have a 1-byte channel prefix
///   (`0x01` = stdout, `0x02` = stderr); text JSON frames are
///   `{"type":"exit","exit_code":N}` (terminal) or
///   `{"type":"error","message":"..."}` (informational, connection stays open).
///
/// Always emits exactly one `ExecResult` to `result_tx` before returning,
/// so `Execution::wait()` can never observe a silent close.
async fn attach_ws(
    client: &ApiClient,
    box_id: &str,
    execution_id: &str,
    stdin_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stdout_tx: mpsc::UnboundedSender<String>,
    stderr_tx: mpsc::UnboundedSender<String>,
    result_tx: mpsc::UnboundedSender<ExecResult>,
) {
    let path = format!("/boxes/{}/executions/{}/attach", box_id, execution_id);
    let stream = match client.connect_ws(&path).await {
        Ok(s) => {
            tracing::debug!(path = %path, "WS attach: connected");
            s
        }
        Err(e) => {
            tracing::debug!(path = %path, error = %e, "WS attach: connect failed; falling back to status probe (output will not stream)");
            emit_or_fallback(
                client,
                box_id,
                execution_id,
                &result_tx,
                format!("WS connect failed: {}", e),
            )
            .await;
            return;
        }
    };
    attach_ws_pump(
        client,
        box_id,
        execution_id,
        stream,
        // exec() creates the session it attaches to, so it is always writable;
        // read-only is only reachable through attach().
        Some(stdin_rx),
        stdout_tx,
        stderr_tx,
        result_tx,
    )
    .await;
}

/// Pump stdin/stdout/stderr/control over a WebSocket attach. On transient
/// disconnects (watchdog timeout, close frame, stream error) the pump probes
/// the server's view of the execution; if the exec is still running it
/// reconnects within `WS_RECONNECT_BUDGET` (aligned with the runner's
/// Phase-4-reaper grace period) before falling back to an error result.
///
/// stdin is forwarded inline via `tokio::select!` instead of a separate
/// spawned task so the WS sink can be replaced on each reconnect without
/// losing buffered stdin bytes.
#[allow(clippy::too_many_arguments)]
async fn attach_ws_pump(
    client: &ApiClient,
    box_id: &str,
    execution_id: &str,
    initial_stream: WsStream,
    // `None` for a read-only attach: there is no writer, so no channel exists.
    mut stdin_rx: Option<mpsc::UnboundedReceiver<Vec<u8>>>,
    stdout_tx: mpsc::UnboundedSender<String>,
    stderr_tx: mpsc::UnboundedSender<String>,
    result_tx: mpsc::UnboundedSender<ExecResult>,
) {
    use futures::{SinkExt, StreamExt};
    use std::time::Instant;
    use tokio_tungstenite::tungstenite::Message;

    // One source of truth for read-only, so the query and the stdin gates below
    // cannot drift from whether a channel actually exists.
    let wire_stdin = stdin_rx.is_some();

    // The query rides every reconnect: a replacement socket opened without it
    // would be writable, and the server's refusal only binds the socket it was
    // asked for.
    let path = format!(
        "/boxes/{}/executions/{}/attach{}",
        box_id,
        execution_id,
        if wire_stdin { "" } else { "?stdin=0" }
    );

    // State persisted across reconnects:
    //
    // - `last_error_message` surfaces the most recent server-reported text
    //   error if we end up emitting a fallback ExecResult.
    // - `user_closed_stdin` remembers whether the SDK consumer dropped its
    //   stdin sender. On reconnect we immediately send `stdin_eof` on the
    //   fresh sink so the new server-side attach gets the same signal.
    let mut last_error_message: Option<String> = None;
    let mut user_closed_stdin = false;
    let mut reconnect_budget = WS_RECONNECT_BUDGET;
    // Sticky across reconnects: once the server has ever sent a frame the
    // exec is real, so a later reconnect uses the steady-state watchdog.
    let mut first_frame_seen = false;

    let mut current_stream = Some(initial_stream);

    'session: loop {
        let stream = match current_stream.take() {
            Some(s) => s,
            None => unreachable!("stream populated at top of loop"),
        };
        let (mut sink, mut read) = stream.split();

        // Client-initiated keepalive for this connection. The branch that
        // fires it is gated on `first_frame_seen`, so the short
        // `WS_FIRST_FRAME_TIMEOUT` fast-fail for never-streaming execs is
        // preserved: we only start pinging once the server has sent a real
        // frame, so the Pong to our own Ping can never be mistaken for it.
        let mut ping_interval = tokio::time::interval(WS_CLIENT_PING_INTERVAL);
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // If the user closed stdin during a previous attach, propagate the
        // EOF to this fresh server-side handler immediately. Best-effort.
        if wire_stdin && user_closed_stdin {
            let _ = sink
                .send(Message::Text(r#"{"type":"stdin_eof"}"#.to_string()))
                .await;
        }

        // Cause that ended the inner loop — used by the reconnect/fallback path.
        let disconnect_cause: String;

        // Inner pump loop. Reads from WS and forwards stdin from the
        // SDK-side channel. Returns by setting `disconnect_cause` and
        // breaking when the WS becomes unusable; returns immediately from
        // the function on a clean Exit frame.
        loop {
            tokio::select! {
                // Forward stdin bytes from the SDK consumer to the WS sink.
                // Disabled once we've observed stdin EOF — the WS reader is
                // still running so we keep waiting for the exit frame.
                stdin_msg = async { stdin_rx.as_mut().expect("guarded by wire_stdin").recv().await },
                    if wire_stdin && !user_closed_stdin => {
                    match stdin_msg {
                        Some(bytes) => {
                            if sink.send(Message::Binary(bytes)).await.is_err() {
                                disconnect_cause = "stdin write failed (sink closed)".to_string();
                                break;
                            }
                        }
                        None => {
                            // SDK consumer dropped the stdin sender.
                            user_closed_stdin = true;
                            let _ = sink
                                .send(Message::Text(r#"{"type":"stdin_eof"}"#.to_string()))
                                .await;
                            // Continue reading — server still owes us an exit frame.
                        }
                    }
                }
                // Client keepalive: once the session is established, ping on a
                // fixed cadence so an idle connection keeps bidirectional
                // traffic flowing and our watchdog is fed by the returned Pong,
                // even if an intermediary swallows the server's own pings.
                _ = ping_interval.tick(), if first_frame_seen => {
                    if sink.send(Message::Ping(Vec::new())).await.is_err() {
                        disconnect_cause =
                            "keepalive ping write failed (sink closed)".to_string();
                        break;
                    }
                }
                next = tokio::time::timeout(
                    if first_frame_seen { WS_WATCHDOG } else { WS_FIRST_FRAME_TIMEOUT },
                    read.next(),
                ) => {
                    let frame = match next {
                        Err(_) => {
                            disconnect_cause = "no WS traffic for watchdog interval (likely connection idle timeout or proxy cut)".to_string();
                            break;
                        }
                        Ok(None) => {
                            disconnect_cause = last_error_message.clone().unwrap_or_else(|| {
                                "WS stream ended before exit frame (likely connection idle timeout or proxy cut)".to_string()
                            });
                            break;
                        }
                        Ok(Some(Err(e))) => {
                            disconnect_cause = format!("WS stream read error: {}", e);
                            break;
                        }
                        Ok(Some(Ok(msg))) => msg,
                    };

                    // Server is talking — switch to the steady-state
                    // idle watchdog for the rest of the session.
                    first_frame_seen = true;

                    tracing::trace!(
                        kind = match &frame {
                            Message::Binary(_) => "binary",
                            Message::Text(_) => "text",
                            Message::Close(_) => "close",
                            Message::Ping(_) => "ping",
                            Message::Pong(_) => "pong",
                            Message::Frame(_) => "raw",
                        },
                        len = frame.len(),
                        "WS attach: frame received",
                    );

                    match frame {
                        Message::Binary(bytes) => {
                            if let Some((channel, payload)) = bytes.split_first() {
                                let text = String::from_utf8_lossy(payload).into_owned();
                                match *channel {
                                    0x01 => {
                                        tracing::trace!(len = text.len(), "WS attach: stdout frame");
                                        let _ = stdout_tx.send(text);
                                    }
                                    0x02 => {
                                        tracing::trace!(len = text.len(), "WS attach: stderr frame");
                                        let _ = stderr_tx.send(text);
                                    }
                                    other => {
                                        tracing::warn!(channel = other, "WS attach: unknown channel prefix");
                                    }
                                }
                            }
                        }
                        Message::Text(text) => match parse_control_frame(&text) {
                            ControlFrame::Exit { exit_code } => {
                                tracing::debug!(exit_code, "WS attach: exit control frame");
                                let _ = result_tx.send(ExecResult {
                                    exit_code,
                                    error_message: None,
                                });
                                return;
                            }
                            ControlFrame::Error { message } => {
                                tracing::warn!(message = %message, "WS attach: server-reported error");
                                last_error_message = Some(message);
                            }
                            ControlFrame::Unknown => {
                                tracing::warn!(text = %text, "WS attach: unrecognized text frame");
                            }
                        },
                        Message::Close(_) => {
                            disconnect_cause = last_error_message.clone().unwrap_or_else(|| {
                                "WS closed before exit frame (likely connection idle timeout or proxy cut)".to_string()
                            });
                            break;
                        }
                        // Pings are auto-replied by tungstenite; pongs/frames just reset the watchdog.
                        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
                    }
                }
            }
        }

        // We disconnected without seeing an Exit frame. Probe the server to
        // distinguish "exec really finished" from "transient WS drop".
        match probe_execution_status(client, box_id, execution_id).await {
            ProbeResult::Terminal(result) => {
                tracing::debug!(
                    cause = %disconnect_cause,
                    "WS attach: disconnected without an exit frame — taking exit code from status probe (any unstreamed stdout/stderr is lost)"
                );
                let _ = result_tx.send(result);
                return;
            }
            ProbeResult::Gone => {
                // Box/exec is definitively gone — fail fast, no reconnect.
                emit_or_fallback(client, box_id, execution_id, &result_tx, disconnect_cause).await;
                return;
            }
            ProbeResult::StillRunning | ProbeResult::Unavailable => {
                // Try to reconnect within the remaining budget.
            }
        }

        // Reconnect attempt loop with exponential backoff.
        let mut backoff = WS_RECONNECT_BACKOFF_INITIAL;
        let reconnect_start = Instant::now();
        loop {
            if reconnect_budget.is_zero() {
                tracing::warn!(
                    box_id,
                    execution_id,
                    cause = %disconnect_cause,
                    "WS attach reconnect budget exhausted",
                );
                emit_or_fallback(client, box_id, execution_id, &result_tx, disconnect_cause).await;
                return;
            }

            let sleep_for = backoff.min(reconnect_budget);
            tokio::time::sleep(sleep_for).await;
            reconnect_budget = reconnect_budget.saturating_sub(sleep_for);

            match client.connect_ws(&path).await {
                Ok(new_stream) => {
                    tracing::info!(
                        box_id,
                        execution_id,
                        reconnect_after_ms = reconnect_start.elapsed().as_millis() as u64,
                        prior_cause = %disconnect_cause,
                        "WS attach reconnected",
                    );
                    current_stream = Some(new_stream);
                    continue 'session;
                }
                Err(e) => {
                    tracing::warn!(
                        box_id,
                        execution_id,
                        error = %e,
                        "WS attach reconnect failed; will retry",
                    );
                    backoff = (backoff * 2).min(WS_RECONNECT_BACKOFF_MAX);
                }
            }
        }
    }
}

/// Outcome of probing `/executions/{id}` after a WS disconnect.
enum ProbeResult {
    /// Server reported a terminal status (`completed`/`killed`/`timed_out`).
    /// The pump should emit this `ExecResult` and stop reconnecting.
    Terminal(ExecResult),
    /// Server reports the exec is still active. Pump should attempt reconnect.
    StillRunning,
    /// Probe failed (timeout, network, etc.). Pump retries reconnect anyway —
    /// the API might be temporarily unavailable but the runner could recover.
    Unavailable,
    /// Server authoritatively says the box/exec does not exist (HTTP 404).
    /// Reconnecting is pointless — there is nothing to reattach to. The
    /// pump must fail fast instead of burning the reconnect budget.
    Gone,
}

/// Probe the server's view of an execution. Mirrors the legacy
/// [`emit_or_fallback`] status query but returns a structured result so the
/// pump can decide whether to reconnect.
async fn probe_execution_status(
    client: &ApiClient,
    box_id: &str,
    execution_id: &str,
) -> ProbeResult {
    let status_path = format!("/boxes/{}/executions/{}", box_id, execution_id);
    let status_probe = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get::<ExecutionStatusResponse>(&status_path),
    );
    match status_probe.await {
        Ok(Ok(info)) => match info.status.as_str() {
            "completed" | "killed" | "timed_out" => ProbeResult::Terminal(ExecResult {
                exit_code: info.exit_code.unwrap_or(-1),
                error_message: None,
            }),
            _ => ProbeResult::StillRunning,
        },
        // A definitive 404 means the box or exec genuinely does not
        // exist — distinct from a transient probe failure. Don't loop
        // the reconnect budget against something that isn't there.
        Ok(Err(BoxliteError::NotFound(_))) => ProbeResult::Gone,
        _ => ProbeResult::Unavailable,
    }
}

/// Decoded form of a Server→Client text-JSON frame.
enum ControlFrame {
    Exit { exit_code: i32 },
    Error { message: String },
    Unknown,
}

fn parse_control_frame(text: &str) -> ControlFrame {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return ControlFrame::Unknown;
    };
    match value.get("type").and_then(|v| v.as_str()) {
        Some("exit") => {
            let exit_code = value
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1) as i32;
            ControlFrame::Exit { exit_code }
        }
        Some("error") => {
            let message = value
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("server reported error without message")
                .to_string();
            ControlFrame::Error { message }
        }
        _ => ControlFrame::Unknown,
    }
}

/// Emit a final `ExecResult` when the WS path terminated without an `exit`
/// frame. Tries the `GET /executions/{id}` status endpoint first so callers
/// observe the real exit code on a silent connection drop; falls back to a
/// synthesized error if the status query is unavailable or still running.
async fn emit_or_fallback(
    client: &ApiClient,
    box_id: &str,
    execution_id: &str,
    result_tx: &mpsc::UnboundedSender<ExecResult>,
    cause: String,
) {
    tracing::debug!(cause = %cause, "WS attach: emit_or_fallback — recovering exit code from status probe (stdout/stderr not streamed)");
    let status_path = format!("/boxes/{}/executions/{}", box_id, execution_id);
    let status_probe = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get::<ExecutionStatusResponse>(&status_path),
    );
    if let Ok(Ok(info)) = status_probe.await {
        match info.status.as_str() {
            "completed" | "killed" | "timed_out" => {
                // The exec finished server-side, but we only reach
                // emit_or_fallback when the output stream failed (never
                // connected, or dropped and couldn't reconnect), so stdout/
                // stderr were not delivered to the caller. Surface the cause as
                // an error_message rather than reporting a clean exit: a caller
                // otherwise can't tell "command produced no output" from "we
                // lost the output". The real exit code is still preserved.
                let _ = result_tx.send(ExecResult {
                    exit_code: info.exit_code.unwrap_or(-1),
                    error_message: Some(cause.clone()),
                });
                return;
            }
            _ => {
                // Server says the exec is still running — surface the
                // synthesized cause so the caller sees the disconnect.
            }
        }
    }
    let _ = result_tx.send(ExecResult {
        exit_code: -1,
        error_message: Some(cause),
    });
}

// ============================================================================
// Tar Helpers
// ============================================================================

/// Read a tar response body into memory, enforcing a size cap.
///
/// Used only as a fallback when the server predates the `source_is_dir`
/// hint and therefore cannot stream end-to-end; past the cap we error
/// instead of buffering unboundedly. Takes the `Response` rather than a
/// byte stream so a severed body keeps its transport classification.
async fn read_capped(resp: reqwest::Response, cap: u64) -> BoxliteResult<Vec<u8>> {
    let mut stream = resp.bytes_stream();
    let mut out: Vec<u8> = Vec::new();
    while let Some(item) = stream.next().await {
        let chunk = item.map_err(transport_error)?;
        if out.len() as u64 + chunk.len() as u64 > cap {
            return Err(BoxliteError::Unsupported(format!(
                "copy_out exceeds {} MiB fallback cap; upgrade the server to stream end-to-end",
                cap / (1024 * 1024)
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Extract a tar archive to a host path.
///
/// When the archive contains exactly one regular file (the common case
/// for `copy_out("/guest/file", "/host/file")`), the file is written
/// at `host_dst` directly so the caller sees the layout they asked
/// for. When the archive contains multiple entries (or any directory
/// entry), `host_dst` is treated as a destination directory and the
/// tree is unpacked into it.
///
/// Pre-fix this always called `archive.unpack(host_dst)`, which
/// always treats `host_dst` as a directory and produces the wrong
/// shape on single-file `copy_out` (callers received `/host/file/`
/// as a directory containing the actual file under it).
fn extract_tar_to_path(tar_bytes: &[u8], host_dst: &Path) -> BoxliteResult<()> {
    // Probe the archive once to decide single-file vs multi-entry.
    // tar::Archive iterators consume the underlying reader, so we
    // re-open a fresh Archive for the actual extraction step.
    let mut probe = tar::Archive::new(tar_bytes);
    let mut file_count = 0usize;
    let mut other_count = 0usize;
    let mut single_entry_path: Option<std::path::PathBuf> = None;
    for entry in probe
        .entries()
        .map_err(|e| BoxliteError::Internal(format!("failed to read tar archive: {}", e)))?
    {
        let entry = entry
            .map_err(|e| BoxliteError::Internal(format!("failed to read tar entry: {}", e)))?;
        let header = entry.header();
        match header.entry_type() {
            tar::EntryType::Regular => {
                file_count += 1;
                if single_entry_path.is_none() {
                    single_entry_path =
                        Some(entry.path().map(|c| c.into_owned()).unwrap_or_default());
                }
            }
            _ => other_count += 1,
        }
    }

    if file_count == 1 && other_count == 0 {
        if let Some(parent) = host_dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Internal(format!(
                    "failed to create directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }
        // Re-read the (single) entry from a fresh Archive and copy
        // its bytes into host_dst directly.
        let mut archive = tar::Archive::new(tar_bytes);
        for entry in archive
            .entries()
            .map_err(|e| BoxliteError::Internal(format!("failed to re-read tar archive: {}", e)))?
        {
            let mut entry = entry.map_err(|e| {
                BoxliteError::Internal(format!("failed to re-read tar entry: {}", e))
            })?;
            if entry.header().entry_type() != tar::EntryType::Regular {
                continue;
            }
            let mut out = std::fs::File::create(host_dst).map_err(|e| {
                BoxliteError::Internal(format!("failed to create {}: {}", host_dst.display(), e))
            })?;
            std::io::copy(&mut entry, &mut out).map_err(|e| {
                BoxliteError::Internal(format!("failed to write {}: {}", host_dst.display(), e))
            })?;
            return Ok(());
        }
        // Probe said one regular file but the second pass found
        // none — defensive fallthrough; shouldn't happen.
        let _ = single_entry_path;
    }

    // Multi-entry archive: treat host_dst as a directory and unpack
    // the tree into it. Matches the historical contract.
    std::fs::create_dir_all(host_dst).map_err(|e| {
        BoxliteError::Internal(format!(
            "failed to create directory {}: {}",
            host_dst.display(),
            e
        ))
    })?;
    let mut archive = tar::Archive::new(tar_bytes);
    archive.unpack(host_dst).map_err(|e| {
        BoxliteError::Internal(format!(
            "failed to extract tar to {}: {}",
            host_dst.display(),
            e
        ))
    })
}

// ============================================================================
// Metrics Conversion
// ============================================================================

/// Convert REST box metrics response to core BoxMetrics.
fn box_metrics_from_response(resp: &BoxMetricsResponse) -> BoxMetrics {
    let (
        total_create_ms,
        guest_boot_ms,
        fs_setup_ms,
        img_prepare_ms,
        guest_rootfs_ms,
        box_config_ms,
        box_spawn_ms,
        container_init_ms,
    ) = if let Some(ref timing) = resp.boot_timing {
        (
            timing.total_create_ms.map(|v| v as u128),
            timing.guest_boot_ms.map(|v| v as u128),
            timing.filesystem_setup_ms.map(|v| v as u128),
            timing.image_prepare_ms.map(|v| v as u128),
            timing.guest_rootfs_ms.map(|v| v as u128),
            timing.box_config_ms.map(|v| v as u128),
            timing.box_spawn_ms.map(|v| v as u128),
            timing.container_init_ms.map(|v| v as u128),
        )
    } else {
        (None, None, None, None, None, None, None, None)
    };

    BoxMetrics {
        commands_executed_total: resp.commands_executed_total,
        exec_errors_total: resp.exec_errors_total,
        bytes_sent_total: resp.bytes_sent_total,
        bytes_received_total: resp.bytes_received_total,
        total_create_duration_ms: total_create_ms,
        guest_boot_duration_ms: guest_boot_ms,
        cpu_percent: resp.cpu_percent,
        memory_bytes: resp.memory_bytes,
        network_bytes_sent: resp.network_bytes_sent,
        network_bytes_received: resp.network_bytes_received,
        network_tcp_connections: resp.network_tcp_connections,
        network_tcp_errors: resp.network_tcp_errors,
        stage_filesystem_setup_ms: fs_setup_ms,
        stage_image_prepare_ms: img_prepare_ms,
        stage_guest_rootfs_ms: guest_rootfs_ms,
        stage_box_config_ms: box_config_ms,
        stage_box_spawn_ms: box_spawn_ms,
        stage_container_init_ms: container_init_ms,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the WebSocket attach pump.
    //!
    //! Each test stands up an in-process TCP listener bound to an ephemeral
    //! port. Per-connection routing inspects the first request line so the
    //! same listener handles both the WS upgrade (`/attach`) and the HTTP
    //! status fallback (`GET /executions/{id}`).

    use super::*;
    use crate::rest::client::ApiClient;
    use crate::rest::options::BoxliteRestOptions;
    use futures::{SinkExt, StreamExt};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{
        ErrorResponse as HandshakeErrorResponse, Request as HandshakeRequest,
        Response as HandshakeResponse,
    };
    use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

    /// Pull the path out of an HTTP request line (`GET /v1/... HTTP/1.1`).
    fn request_path(head: &str) -> Option<String> {
        head.lines()
            .next()?
            .split_whitespace()
            .nth(1)
            .map(str::to_string)
    }

    /// Recorded behavior of the in-process test server.
    #[derive(Default)]
    struct ServerState {
        /// Binary frames the server received from the client (stdin bytes).
        received_stdin: Vec<Vec<u8>>,
        /// Whether `GET /executions/{id}` was hit and what we replied with.
        status_calls: u32,
        /// Request-line paths the client asked for, in order.
        requested_paths: Vec<String>,
        /// Text (control) frames the server received from the client.
        received_text: Vec<String>,
        /// Headers to add to the WS handshake response — how the real
        /// server hands back the main session's execution id.
        ws_response_headers: Vec<(String, String)>,
        /// When set, reject the WS upgrade with this HTTP status instead of
        /// completing it — models the server refusing a reattach (404 reaped,
        /// 409 already-attached).
        reject_upgrade_status: Option<u16>,
    }

    /// Shorthand for the `Arc<Mutex<...>>` shared between server and client.
    type SharedState = Arc<Mutex<ServerState>>;

    /// Read the first HTTP request line + headers off a freshly accepted TCP
    /// stream. Returns the raw bytes consumed (so the WS upgrade can resume
    /// from where we left off if needed) and a parsed view of the request.
    async fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1024);
        let mut tmp = [0u8; 512];
        loop {
            let n = match stream.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
            if buf.len() > 16 * 1024 {
                break;
            }
        }
        buf
    }

    /// Build an `ApiClient` pointed at `127.0.0.1:{port}`.
    fn client_for(port: u16) -> ApiClient {
        let opts = BoxliteRestOptions::new(format!("http://127.0.0.1:{}", port));
        ApiClient::new(&opts).expect("ApiClient::new")
    }

    /// A `RestBox` for `box_id`, pointed at the in-process server. The
    /// `BoxInfo` comes out of the production wire decoder rather than a
    /// hand-rolled literal.
    fn rest_box_for(port: u16, box_id: &str) -> RestBox {
        let body = format!(
            r#"{{"box_id":"{box_id}","name":null,"status":"running",
                 "created_at":"2026-07-14T00:00:00Z",
                 "updated_at":"2026-07-14T00:00:00Z",
                 "pid":null,"image":"alpine:latest","cpus":1,"memory_mib":512}}"#
        );
        let resp: BoxResponse = serde_json::from_str(&body).expect("BoxResponse");
        RestBox::new(client_for(port), resp.to_box_info().expect("to_box_info"))
    }

    /// Send a minimal HTTP/1.1 200 OK with a JSON body.
    async fn write_status_response(stream: &mut TcpStream, body: &str) {
        let resp = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    /// Stream wrapper that replays a buffered prefix before delegating to the
    /// underlying TcpStream. Lets us peek at the HTTP request line for
    /// routing while still letting `accept_async` re-parse it.
    struct ChainedStream {
        head: Vec<u8>,
        head_pos: usize,
        inner: TcpStream,
    }

    impl tokio::io::AsyncRead for ChainedStream {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.head_pos < self.head.len() {
                let remaining = &self.head[self.head_pos..];
                let take = remaining.len().min(buf.remaining());
                buf.put_slice(&remaining[..take]);
                self.head_pos += take;
                return std::task::Poll::Ready(Ok(()));
            }
            std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncWrite for ChainedStream {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize, std::io::Error>> {
            std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::pin::Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// Install the WS server: peek the head, route by Upgrade header, run
    /// the per-connection handler. Subsequent connections (after the WS
    /// upgrade is consumed) reply with `status_body` if provided so the
    /// `attach_ws` status fallback path can be exercised end-to-end.
    ///
    /// The loop runs until the listener is dropped (`server.abort()` from
    /// the test) — never `return`s on its own — so status probes that
    /// arrive AFTER the WS connection closes still get answered.
    // The handshake callback's Err type is tungstenite's `ErrorResponse`
    // (a full http::Response); its size is not ours to shrink.
    #[allow(clippy::result_large_err)]
    async fn run_server<F, Fut>(
        listener: TcpListener,
        state: SharedState,
        status_body: Option<String>,
        ws_handler: F,
    ) where
        F: FnOnce(tokio_tungstenite::WebSocketStream<ChainedStream>, SharedState) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut ws_handler = Some(ws_handler);
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => return,
            };
            let head = read_request_head(&mut stream).await;
            let head_str = String::from_utf8_lossy(&head);
            if let Some(path) = request_path(&head_str) {
                state.lock().await.requested_paths.push(path);
            }
            let is_upgrade = head_str.to_ascii_lowercase().contains("upgrade: websocket");
            if is_upgrade {
                if let Some(handler) = ws_handler.take() {
                    let chained = ChainedStream {
                        head,
                        head_pos: 0,
                        inner: stream,
                    };
                    let extra_headers = state.lock().await.ws_response_headers.clone();
                    let reject_status = state.lock().await.reject_upgrade_status;
                    let with_headers = |_req: &HandshakeRequest, mut resp: HandshakeResponse| {
                        if let Some(code) = reject_status {
                            let err: HandshakeErrorResponse =
                                tokio_tungstenite::tungstenite::http::Response::builder()
                                    .status(code)
                                    .body(Some(format!("upgrade rejected with {code}")))
                                    .expect("error response");
                            return Err(err);
                        }
                        for (name, value) in extra_headers {
                            resp.headers_mut().insert(
                                HeaderName::from_bytes(name.as_bytes()).expect("header name"),
                                HeaderValue::from_str(&value).expect("header value"),
                            );
                        }
                        Ok(resp)
                    };
                    match tokio_tungstenite::accept_hdr_async(chained, with_headers).await {
                        Ok(ws) => handler(ws, state.clone()).await,
                        Err(_) => continue,
                    }
                }
                // Already handled the upgrade once; subsequent ones close.
            } else if let Some(ref body) = status_body {
                let mut s = state.lock().await;
                s.status_calls += 1;
                drop(s);
                write_status_response(&mut stream, body).await;
            } else {
                let _ = stream.shutdown().await;
            }
        }
    }

    #[tokio::test]
    async fn info_fetches_current_remote_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        let state_clone = state.clone();
        let body = r#"{"box_id":"box1","name":null,"status":"stopped",
                       "created_at":"2026-07-14T00:00:00Z",
                       "updated_at":"2026-07-15T00:00:00Z",
                       "pid":null,"image":"alpine:latest","cpus":1,"memory_mib":512}"#
            .to_string();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, Some(body), |_ws, _state| async {}).await;
        });
        let rest_box = rest_box_for(port, "box1");

        let info = rest_box.info().await.expect("fetch box info");

        assert_eq!(info.status, crate::BoxStatus::Stopped);
        assert_eq!(
            state.lock().await.requested_paths,
            vec!["/v1/boxes/box1".to_string()]
        );
        server.abort();
    }

    /// A download whose connection dies mid-body is a transport fault, not a
    /// client bug — `copy_out` must classify it as such so a caller can retry
    /// instead of filing one.
    #[tokio::test]
    async fn copy_out_reports_a_truncated_download_as_network() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            // Promise 4 KiB of tar, deliver two bytes, then hang up.
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: application/x-tar\r\n\
                      Content-Length: 4096\r\n\
                      Connection: close\r\n\r\nhi",
                )
                .await
                .unwrap();
        });

        let rest_box = rest_box_for(port, "box1");
        let host_dst = std::env::temp_dir().join("boxlite-copy-out-truncated");

        let error = rest_box
            .copy_out("/tmp/archive", &host_dst, CopyOptions::default())
            .await
            .expect_err("a truncated download must fail");

        assert!(
            matches!(&error, BoxliteError::Network(_)),
            "a severed download must classify as transport, got {error:?}"
        );
        server.await.unwrap();
    }

    /// The streaming path (server sends the shape hint) must preserve the same
    /// transport-fault classification the buffered fallback gives. Exercises the
    /// shared fault slot, not just `read_capped`.
    #[tokio::test]
    async fn copy_out_streaming_truncation_classifies_as_network() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            // Streaming peer: carries the shape hint, promises 4 KiB, hangs up.
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: application/x-tar\r\n\
                      X-Boxlite-Source-Is-Dir: false\r\n\
                      Content-Length: 4096\r\n\
                      Connection: close\r\n\r\nhi",
                )
                .await
                .unwrap();
        });

        let rest_box = rest_box_for(port, "box1");
        let host_dst = std::env::temp_dir().join("boxlite-copy-out-stream-truncated");

        let error = rest_box
            .copy_out("/tmp/archive", &host_dst, CopyOptions::default())
            .await
            .expect_err("a truncated streaming download must fail");

        assert!(
            matches!(&error, BoxliteError::Network(_)),
            "a severed streaming download must classify as transport, got {error:?}"
        );
        server.await.unwrap();
    }

    /// Streaming copy_out must fold the destination's directory-ness back into
    /// the archive shape: a single-file source whose destination is an existing
    /// directory lands *inside* it (Unix cp semantics), not at the directory
    /// path — mirroring `detect_extraction_mode` in the legacy path.
    #[tokio::test]
    async fn copy_out_places_single_file_inside_existing_directory() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // A single-file tar: one regular entry "file.txt".
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let content = b"inside-dir\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "file.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/x-tar\r\n\
                         X-Boxlite-Source-Is-Dir: false\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        tar_bytes.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&tar_bytes).await.unwrap();
        });

        let rest_box = rest_box_for(port, "box1");
        let dir = tempfile::tempdir().unwrap();

        rest_box
            .copy_out("/tmp/file.txt", dir.path(), CopyOptions::default())
            .await
            .expect("copy_out into existing directory");

        let landed = dir.path().join("file.txt");
        assert_eq!(
            std::fs::read_to_string(&landed).unwrap(),
            "inside-dir\n",
            "single file must land inside the destination directory"
        );
        server.await.unwrap();
    }

    /// A destination with a trailing slash names a directory even when it
    /// does not exist yet: a single-file source must land *inside* it (the
    /// legacy `detect_extraction_mode` trailing-slash rule), not fail to
    /// unpack at the directory-designated path.
    #[tokio::test]
    async fn copy_out_trailing_slash_forces_directory_mode() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // A single-file tar: one regular entry "file.txt".
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let content = b"trailing-slash\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "file.txt", &content[..])
                .unwrap();
            builder.finish().unwrap();
        }

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/x-tar\r\n\
                         X-Boxlite-Source-Is-Dir: false\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        tar_bytes.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(&tar_bytes).await.unwrap();
        });

        let rest_box = rest_box_for(port, "box1");
        let dir = tempfile::tempdir().unwrap();

        // The trailing slash makes this a directory even though it does not
        // exist yet (mkdir_parents creates it).
        let dest_with_slash =
            std::path::PathBuf::from(format!("{}/", dir.path().join("newdir").display()));
        rest_box
            .copy_out("/tmp/file.txt", &dest_with_slash, CopyOptions::default())
            .await
            .expect("copy_out with trailing-slash destination");

        let landed = dir.path().join("newdir").join("file.txt");
        assert_eq!(
            std::fs::read_to_string(&landed).unwrap(),
            "trailing-slash\n",
            "single file must land inside the trailing-slash directory"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn info_rejects_a_mismatched_remote_box_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        let state_clone = state.clone();
        let body = r#"{"box_id":"box2","name":null,"status":"stopped",
                       "created_at":"2026-07-14T00:00:00Z",
                       "updated_at":"2026-07-15T00:00:00Z",
                       "pid":null,"image":"alpine:latest","cpus":1,"memory_mib":512}"#
            .to_string();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, Some(body), |_ws, _state| async {}).await;
        });
        let rest_box = rest_box_for(port, "box1");

        let error = rest_box.info().await.expect_err("reject mismatched box id");

        assert!(
            error
                .to_string()
                .contains("box2 does not match handle box1")
        );
        assert_eq!(rest_box.id().as_str(), "box1");
        assert_eq!(rest_box.box_id_str(), "box1");
        assert_eq!(
            state.lock().await.requested_paths,
            vec!["/v1/boxes/box1".to_string()]
        );
        server.abort();
    }

    // ─── ws_clean_exit_emits_result ───────────────────────────────────────
    //
    // Server sends one stdout binary frame, one exit text frame, then
    // closes. Client must observe `ExecResult { exit_code: 7 }`, the
    // stdout payload, and stdin bytes must round-trip back as binary.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_clean_exit_emits_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, None, |mut ws, state| async move {
                // Drain at least one stdin frame BEFORE sending exit so the
                // assertion below has something to observe — without this
                // ordering the client may abort its stdin pump before the
                // bytes traverse the WS.
                if let Some(Ok(Message::Binary(b))) = ws.next().await {
                    let mut s = state.lock().await;
                    s.received_stdin.push(b);
                }
                ws.send(Message::Binary(vec![0x01, b'h', b'i']))
                    .await
                    .unwrap();
                ws.send(Message::Text(r#"{"type":"exit","exit_code":7}"#.into()))
                    .await
                    .unwrap();
                let _ = ws.close(None).await;
            })
            .await;
        });

        let client = client_for(port);
        let (stdout_tx, mut stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<String>();
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (result_tx, mut result_rx) = mpsc::unbounded_channel::<ExecResult>();

        // Push stdin before the pump runs; it'll be drained as soon as
        // the WS connection is up.
        stdin_tx.send(b"hello".to_vec()).unwrap();

        let attach = tokio::spawn(async move {
            attach_ws(
                &client, "box1", "exec1", stdin_rx, stdout_tx, stderr_tx, result_tx,
            )
            .await;
        });

        let res = tokio::time::timeout(Duration::from_secs(3), result_rx.recv())
            .await
            .expect("result channel timed out")
            .expect("result channel closed without value");
        assert_eq!(res.exit_code, 7);
        assert!(res.error_message.is_none());

        let out = tokio::time::timeout(Duration::from_secs(1), stdout_rx.recv())
            .await
            .expect("stdout timed out")
            .expect("stdout channel closed");
        assert_eq!(out, "hi");

        attach.await.unwrap();
        let s = state.lock().await;
        assert!(
            s.received_stdin.iter().any(|b| b == b"hello"),
            "server never observed stdin payload; got {:?}",
            s.received_stdin
        );
        drop(s);
        server.abort();
    }

    // ─── ws_close_without_exit_falls_back_to_status ──────────────────────
    //
    // Server sends one stdout frame, then closes WITHOUT an exit frame.
    // The client must hit `GET /executions/{id}` and surface the real exit
    // code (42) from that response — never a generic "stream closed" error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_close_without_exit_falls_back_to_status() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        let status_body =
            r#"{"execution_id":"exec1","status":"completed","exit_code":42}"#.to_string();
        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(
                listener,
                state_clone,
                Some(status_body),
                |mut ws, _state| async move {
                    ws.send(Message::Binary(vec![0x01, b'x'])).await.unwrap();
                    let _ = ws.close(None).await;
                },
            )
            .await;
        });

        let client = client_for(port);
        let (stdout_tx, _stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<String>();
        let (_stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (result_tx, mut result_rx) = mpsc::unbounded_channel::<ExecResult>();

        let attach = tokio::spawn(async move {
            attach_ws(
                &client, "box1", "exec1", stdin_rx, stdout_tx, stderr_tx, result_tx,
            )
            .await;
        });

        let res = tokio::time::timeout(Duration::from_secs(3), result_rx.recv())
            .await
            .expect("result channel timed out")
            .expect("result channel closed without value");
        assert_eq!(
            res.exit_code, 42,
            "expected status fallback to surface real exit code"
        );
        assert!(res.error_message.is_none());

        attach.await.unwrap();
        let s = state.lock().await;
        assert!(
            s.status_calls >= 1,
            "status fallback endpoint was never called"
        );
        drop(s);
        server.abort();
    }

    // ─── ws_watchdog_fires_when_idle ─────────────────────────────────────
    //
    // Server accepts the upgrade and then goes silent. The cfg(test)
    // override keeps WS_WATCHDOG short (~300 ms) so this test finishes
    // promptly. The emitted ExecResult must name the watchdog as cause —
    // otherwise we've regressed the silent-stall protection.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_watchdog_fires_when_idle() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, None, |ws, _state| async move {
                // Hold the connection open without sending anything.
                // Wait long enough for the client watchdog to fire and
                // close the WS from its side.
                let _kept_alive = ws;
                tokio::time::sleep(Duration::from_secs(2)).await;
            })
            .await;
        });

        let client = client_for(port);
        let (stdout_tx, _stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<String>();
        let (_stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (result_tx, mut result_rx) = mpsc::unbounded_channel::<ExecResult>();

        let attach = tokio::spawn(async move {
            attach_ws(
                &client, "box1", "exec1", stdin_rx, stdout_tx, stderr_tx, result_tx,
            )
            .await;
        });

        let res = tokio::time::timeout(Duration::from_secs(3), result_rx.recv())
            .await
            .expect("watchdog never fired")
            .expect("result channel closed without value");
        assert_eq!(res.exit_code, -1);
        let msg = res.error_message.expect("expected diagnostic message");
        assert!(msg.contains("watchdog"), "unexpected diagnostic: {:?}", msg);

        attach.await.unwrap();
        server.abort();
    }

    // ─── attach_streams_main_session_over_container_route ─────────────────
    //
    // `boxlite run IMAGE COMMAND` against a REST runtime attaches to the
    // box's MAIN session — COMMAND *is* the container init now (docker
    // semantics), so `run` calls RestBox::attach(), not exec(). Before
    // this existed the REST backend inherited the trait's `Unsupported`
    // default and `run` failed outright against `--url` / a stored profile.
    //
    // The attach must address the container route (exec-attach cannot
    // reach init: the client has no id for it), adopt the execution id the
    // server hands back on the upgrade, and stream init's output and exit code.
    /// A read-only attach must survive a dropped socket still read-only.
    ///
    /// Its stdin sender is dropped by construction, which the pump used to read
    /// as the consumer closing stdin — so it announced `stdin_eof`, and the
    /// reconnect rebuilt the URL without the query, giving that EOF a writable
    /// socket to land on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_only_attach_stays_read_only_across_a_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        state.lock().await.ws_response_headers = vec![(
            "x-boxlite-execution-id".to_string(),
            "container-1".to_string(),
        )];

        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, None, |mut ws, state| async move {
                // Record whatever the pump volunteers, then drop the socket so
                // it has to reconnect.
                let _ = tokio::time::timeout(Duration::from_millis(300), async {
                    while let Some(Ok(msg)) = ws.next().await {
                        if let Message::Text(text) = msg {
                            state.lock().await.received_text.push(text.to_string());
                        }
                    }
                })
                .await;
                let _ = ws.close(None).await;
            })
            .await;
        });

        let rest_box = rest_box_for(port, "box1");
        let _execution = tokio::time::timeout(
            Duration::from_secs(3),
            rest_box.attach(AttachOptions::main().read_only()),
        )
        .await
        .expect("attach timed out")
        .expect("a read-only attach must connect");

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            let seen = state
                .lock()
                .await
                .requested_paths
                .iter()
                .filter(|p| p.contains("/attach"))
                .count();
            if seen >= 2 || std::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let seen = state.lock().await;
        let attaches: Vec<&String> = seen
            .requested_paths
            .iter()
            .filter(|p| p.contains("/attach"))
            .collect();
        assert!(
            attaches.len() >= 2,
            "the pump must reconnect after the drop; saw {attaches:?}"
        );
        for path in &attaches {
            assert!(
                path.contains("stdin=0"),
                "every attach of a read-only session must carry the query, got {path:?}"
            );
        }
        assert!(
            seen.received_text.is_empty(),
            "a read-only attach has no stdin to close and must never announce one, got {:?}",
            seen.received_text
        );

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_streams_main_session_over_container_route() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        state.lock().await.ws_response_headers = vec![(
            "x-boxlite-execution-id".to_string(),
            "container-1".to_string(),
        )];

        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, None, |mut ws, _state| async move {
                ws.send(Message::Binary(vec![0x01, b'h', b'i']))
                    .await
                    .unwrap();
                ws.send(Message::Text(r#"{"type":"exit","exit_code":7}"#.into()))
                    .await
                    .unwrap();
                let _ = ws.close(None).await;
            })
            .await;
        });

        let rest_box = rest_box_for(port, "box1");
        let mut execution = tokio::time::timeout(
            Duration::from_secs(3),
            rest_box.attach(AttachOptions::main()),
        )
        .await
        .expect("attach timed out")
        .expect("the REST backend must support attaching to the main command session");

        assert_eq!(
            execution.id(),
            "container-1",
            "the main session must adopt the execution id the server assigned on the upgrade",
        );

        let mut stdout = execution.stdout().expect("stdout stream");
        let line = tokio::time::timeout(Duration::from_secs(3), stdout.next())
            .await
            .expect("stdout timed out")
            .expect("stdout closed without data");
        assert_eq!(line, "hi", "init's output must reach the caller");

        let result = tokio::time::timeout(Duration::from_secs(3), execution.wait())
            .await
            .expect("wait timed out")
            .expect("wait");
        assert_eq!(
            result.exit_code, 7,
            "init's exit code must propagate (it becomes `run`'s exit code)",
        );

        let recorded = state.lock().await;
        assert_eq!(
            recorded.requested_paths,
            vec!["/v1/boxes/box1/attach".to_string()],
            "attach must address the container-attach route",
        );
        drop(recorded);
        server.abort();
    }

    // ─── attach_some_id_reattaches_over_the_exec_route ────────────────────
    //
    // `attach(Some(id))` folds in what used to be `attach_exec`: reattaching to
    // an already-running exec after a transport drop. It must address the
    // *exec*-attach route (not the container route), keep the id the caller
    // named — there is no upgrade header to adopt, because the caller owns the
    // id — and stream the session's output.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_some_id_reattaches_over_the_exec_route() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));

        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, None, |mut ws, _state| async move {
                ws.send(Message::Binary(vec![0x01, b'o', b'k']))
                    .await
                    .unwrap();
                ws.send(Message::Text(r#"{"type":"exit","exit_code":0}"#.into()))
                    .await
                    .unwrap();
                let _ = ws.close(None).await;
            })
            .await;
        });

        let rest_box = rest_box_for(port, "box1");
        let mut execution = tokio::time::timeout(
            Duration::from_secs(3),
            rest_box.attach(AttachOptions::execution("exec-9")),
        )
        .await
        .expect("attach timed out")
        .expect("the REST backend must support reattaching to an exec by id");

        assert_eq!(
            execution.id(),
            "exec-9",
            "a reattach keeps the exec id the caller named, not one from a header",
        );

        let mut stdout = execution.stdout().expect("stdout stream");
        let line = tokio::time::timeout(Duration::from_secs(3), stdout.next())
            .await
            .expect("stdout timed out")
            .expect("stdout closed without data");
        assert_eq!(
            line, "ok",
            "the reattached session's output must reach the caller",
        );

        let recorded = state.lock().await;
        assert_eq!(
            recorded.requested_paths,
            vec!["/v1/boxes/box1/executions/exec-9/attach".to_string()],
            "reattach must address the exec-attach route, not the container route",
        );
        drop(recorded);
        server.abort();
    }

    // ─── attach_none_without_session_header_is_unsupported ────────────────
    //
    // The main session's id is not nameable by the client — the server hands it
    // back on the upgrade header. A server that upgrades without it (one that
    // predates container-attach) must surface as `Unsupported`, not a silent
    // `Execution` wired to an empty id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_none_without_session_header_is_unsupported() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // `ws_response_headers` left empty: the server upgrades but names no id.
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));

        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, None, |ws, _state| async move {
                // Keep the socket open long enough for the client to inspect the
                // handshake response; it rejects on the missing header, not a frame.
                let _hold = ws;
                tokio::time::sleep(Duration::from_millis(200)).await;
            })
            .await;
        });

        let rest_box = rest_box_for(port, "box1");
        let err = match tokio::time::timeout(
            Duration::from_secs(3),
            rest_box.attach(AttachOptions::main()),
        )
        .await
        {
            Ok(Ok(_)) => {
                panic!("attach(None) must fail when the server names no main session id")
            }
            Ok(Err(e)) => e,
            Err(_) => panic!("attach timed out"),
        };
        assert!(
            matches!(err, BoxliteError::Unsupported(_)),
            "a missing session-id header must surface as Unsupported, got: {err:?}",
        );
        server.abort();
    }

    // ─── attach_some_id_maps_reaped_and_conflict_to_typed_errors ──────────
    //
    // Reattaching to an exec the server has reaped (404) must surface as
    // `SessionReaped`, and one another client already holds (409) as
    // `AlreadyExists` — the two outcomes the SDK's reconnect logic branches on.
    // The rejection rides the WS *upgrade*, so it lands at the caller's await,
    // not in a later `wait()`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_some_id_maps_reaped_and_conflict_to_typed_errors() {
        for (status, expect_reaped) in [(404u16, true), (409u16, false)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
            state.lock().await.reject_upgrade_status = Some(status);

            let state_clone = state.clone();
            let server = tokio::spawn(async move {
                run_server(listener, state_clone, None, |ws, _state| async move {
                    let _ = ws; // never reached: the upgrade itself is rejected
                })
                .await;
            });

            let rest_box = rest_box_for(port, "box1");
            let err = match tokio::time::timeout(
                Duration::from_secs(3),
                rest_box.attach(AttachOptions::execution("exec-x")),
            )
            .await
            {
                Ok(Ok(_)) => {
                    panic!("a rejected upgrade ({status}) must not yield an Execution")
                }
                Ok(Err(e)) => e,
                Err(_) => panic!("attach timed out"),
            };

            if expect_reaped {
                assert!(
                    matches!(err, BoxliteError::SessionReaped(_)),
                    "a 404 reattach must map to SessionReaped, got: {err:?}",
                );
            } else {
                assert!(
                    matches!(err, BoxliteError::AlreadyExists(_)),
                    "a 409 reattach must map to AlreadyExists, got: {err:?}",
                );
            }
            server.abort();
        }
    }

    // ─── ws_client_keepalive_pings_idle_session ──────────────────────────
    //
    // Regression for the idle-disconnect bug (POL-120). Once a session is
    // established, the client must send its OWN periodic WS Ping so an idle
    // interactive exec keeps bidirectional traffic — otherwise an intermediary
    // that silently drops the server's keepalive pings lets the client
    // `WS_WATCHDOG` trip on a perfectly healthy connection.
    //
    // The server flips `first_frame_seen` with one stdout frame, then stays
    // quiet and waits for the client's Ping (tungstenite surfaces it here while
    // auto-replying the Pong), signalling a oneshot when it arrives. Without
    // the client keepalive that Ping never comes, the oneshot never fires, and
    // the assertion pinpoints the missing keepalive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_client_keepalive_pings_idle_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));

        // Fires when the server observes the client's first keepalive Ping.
        let (ping_tx, ping_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            run_server(listener, state, None, move |mut ws, _state| async move {
                let mut ping_tx = Some(ping_tx);
                // One stdout frame establishes the session (first_frame_seen),
                // the precondition for the client to begin keepalive pings.
                ws.send(Message::Binary(vec![0x01, b'o', b'k']))
                    .await
                    .unwrap();
                // Stay quiet and wait for the client's keepalive Ping.
                while let Some(Ok(frame)) = ws.next().await {
                    if let Message::Ping(_) = frame {
                        if let Some(tx) = ping_tx.take() {
                            let _ = tx.send(());
                        }
                        let _ = ws
                            .send(Message::Text(r#"{"type":"exit","exit_code":0}"#.into()))
                            .await;
                        let _ = ws.close(None).await;
                        return;
                    }
                }
            })
            .await;
        });

        let client = client_for(port);
        let (stdout_tx, _stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<String>();
        let (_stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (result_tx, _result_rx) = mpsc::unbounded_channel::<ExecResult>();

        let attach = tokio::spawn(async move {
            attach_ws(
                &client, "box1", "exec1", stdin_rx, stdout_tx, stderr_tx, result_tx,
            )
            .await;
        });

        // The established session is idle, so only a client-initiated Ping can
        // keep it alive end-to-end. 3s is well above the test ping interval
        // (100ms) and watchdog (300ms): a healthy client pings comfortably
        // inside it, while a client that never pings leaves the oneshot unfired.
        let pinged = tokio::time::timeout(Duration::from_secs(3), ping_rx).await;
        assert!(
            matches!(pinged, Ok(Ok(()))),
            "client sent no keepalive WS ping on the idle session (waited 3s, got {:?})",
            pinged,
        );

        attach.abort();
        server.abort();
    }

    // ─── ws_text_error_frame_logs_but_continues ──────────────────────────
    //
    // An informational `error` text frame must NOT terminate the
    // connection. Only the subsequent `exit` frame does. This guards
    // against treating a recoverable signal-rejection as a terminal error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ws_text_error_frame_logs_but_continues() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state: SharedState = Arc::new(Mutex::new(ServerState::default()));
        let state_clone = state.clone();
        let server = tokio::spawn(async move {
            run_server(listener, state_clone, None, |mut ws, _state| async move {
                ws.send(Message::Text(
                    r#"{"type":"error","message":"signal not allowed"}"#.into(),
                ))
                .await
                .unwrap();
                ws.send(Message::Text(r#"{"type":"exit","exit_code":0}"#.into()))
                    .await
                    .unwrap();
                let _ = ws.close(None).await;
            })
            .await;
        });

        let client = client_for(port);
        let (stdout_tx, _stdout_rx) = mpsc::unbounded_channel::<String>();
        let (stderr_tx, _stderr_rx) = mpsc::unbounded_channel::<String>();
        let (_stdin_tx, stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (result_tx, mut result_rx) = mpsc::unbounded_channel::<ExecResult>();

        let attach = tokio::spawn(async move {
            attach_ws(
                &client, "box1", "exec1", stdin_rx, stdout_tx, stderr_tx, result_tx,
            )
            .await;
        });

        let res = tokio::time::timeout(Duration::from_secs(3), result_rx.recv())
            .await
            .expect("result channel timed out")
            .expect("result channel closed without value");
        assert_eq!(
            res.exit_code, 0,
            "informational error frame must not terminate the attach"
        );
        assert!(res.error_message.is_none());

        attach.await.unwrap();
        server.abort();
    }

    /// Answer `requests` requests with the error envelope the server really
    /// emits for a refused copy: `Unsupported` → HTTP 400, `code:
    /// "unsupported"`.
    ///
    /// The request is drained before replying because `copy_into` sends a tar
    /// body — answering mid-upload gives the client a transport error instead
    /// of the status under test.
    async fn serve_refusal_envelope(listener: TcpListener, requests: usize) {
        const REFUSAL: &str = concat!(
            r#"{"error":{"type":"UnsupportedError","code":"unsupported","message":"#,
            r#""/tmp/x is under the container's '/tmp' mount, which file transfer cannot reach"}}"#
        );

        for _ in 0..requests {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(stream.read_u8().await.unwrap());
            }
            let content_length = String::from_utf8_lossy(&head)
                .to_lowercase()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            let mut body = vec![0u8; content_length];
            stream.read_exact(&mut body).await.unwrap();

            let resp = format!(
                "HTTP/1.1 400 Bad Request\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{}",
                REFUSAL.len(),
                REFUSAL
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    }

    /// A refusal the server states plainly must not reach the caller as a
    /// server fault.
    ///
    /// Every other REST call decodes a failed response through `ApiClient`,
    /// which reconstructs the `BoxliteError` the server named. `copy_into` and
    /// `copy_out` read the body themselves and flattened all of it to
    /// `Internal` — a 500 for a 400 the caller caused. A mount-shadowed path is
    /// the case that makes it matter: it is a routine outcome now, all three
    /// SDKs document it as a refusal, and `Internal` is exactly what
    /// `map_tonic_err` was fixed to stop producing for it on the FFI backend.
    #[tokio::test]
    async fn a_refused_copy_keeps_the_servers_error_class() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve_refusal_envelope(listener, 2));

        let bx = rest_box_for(port, "box1");
        let tmp = tempfile::tempdir().unwrap();

        let out_err = bx
            .copy_out(
                "/tmp/x",
                &tmp.path().join("out.txt"),
                CopyOptions::default(),
            )
            .await
            .expect_err("a 400 refusal must not read as success");
        assert!(
            matches!(out_err, BoxliteError::Unsupported(_)),
            "copy_out flattened the refusal: {out_err:?}"
        );
        assert!(
            out_err.to_string().contains("'/tmp' mount"),
            "the refusal text must survive: {out_err}"
        );

        let src = tmp.path().join("payload.txt");
        std::fs::write(&src, b"payload").unwrap();
        let in_err = bx
            .copy_into(&src, "/tmp/x", CopyOptions::default())
            .await
            .expect_err("a 400 refusal must not read as success");
        assert!(
            matches!(in_err, BoxliteError::Unsupported(_)),
            "copy_into flattened the refusal: {in_err:?}"
        );

        server.await.unwrap();
    }

    // ── Streaming copy over REST ──────────────────────────────────
    //
    // `copy_out_stream` hands the archive to its caller instead of unpacking
    // it, so it has nowhere to spool a shapeless archive and must report the
    // server's answer verbatim. Getting that wrong is silent: a fabricated
    // `false` extracts a directory tree as one file.

    /// Serve a tar body, with or without the shape header.
    async fn serve_archive(header: Option<&'static str>) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await.unwrap());
            }
            let shape = match header {
                Some(value) => format!("X-Boxlite-Source-Is-Dir: {value}\r\n"),
                None => String::new(),
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/x-tar\r\n\
                         {shape}Content-Length: 5\r\n\
                         Connection: close\r\n\r\nabcde"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        (port, server)
    }

    async fn stream_out(header: Option<&'static str>) -> (Vec<u8>, CopySourceKind) {
        let (port, server) = serve_archive(header).await;
        let rest_box = Arc::new(rest_box_for(port, "box1"));

        let (mut stream, source) = rest_box
            .copy_out_stream("/tmp/archive".to_string(), CopyOptions::default())
            .await
            .expect("copy_out_stream");
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
        server.await.unwrap();
        (bytes, source)
    }

    #[tokio::test]
    async fn copy_out_stream_reports_the_shape_the_server_sent() {
        let (bytes, dir) = stream_out(Some("true")).await;
        assert_eq!(dir, CopySourceKind::Dir);
        assert_eq!(bytes, b"abcde");

        let (_, file) = stream_out(Some("false")).await;
        assert_eq!(file, CopySourceKind::File);
    }

    /// A server predating the header — and, on the hosted path, any proxy that
    /// drops it. There is no destination to spool to here, so the honest answer
    /// is `Unknown`; the bytes must still arrive.
    #[tokio::test]
    async fn copy_out_stream_reports_unknown_when_the_server_omits_the_header() {
        let (bytes, source) = stream_out(None).await;

        assert_eq!(source, CopySourceKind::Unknown);
        assert_eq!(bytes, b"abcde");
    }

    /// `overwrite=false` cannot be expressed on this wire, so the refusal has
    /// to come *before* the upload starts — a request that reached the server
    /// would clobber what the caller asked us to preserve.
    #[tokio::test]
    async fn copy_in_stream_refuses_no_overwrite_without_sending_anything() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_request = Arc::clone(&served);
        let server = tokio::spawn(async move {
            if listener.accept().await.is_ok() {
                saw_request.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });

        let rest_box = Arc::new(rest_box_for(port, "box1"));
        let archive: BoxByteStream = Box::pin(futures::stream::empty());
        let error = rest_box
            .copy_in_stream(
                archive,
                "/tmp/dst".to_string(),
                CopySourceKind::File,
                CopyOptions::default().no_overwrite(),
            )
            .await
            .expect_err("overwrite=false must be refused");

        assert!(matches!(error, BoxliteError::Unsupported(_)), "{error:?}");
        assert!(
            !served.load(std::sync::atomic::Ordering::SeqCst),
            "the refusal must happen before any request is sent"
        );
        server.abort();
    }
}
