//! Files service interface.
//!
//! Provides streaming upload/download to the guest container rootfs.

use crate::litebox::CopySourceKind;
use boxlite_shared::{
    BoxByteStream, BoxliteError, BoxliteResult, DownloadRequest, FilesClient, UploadChunk,
};
use futures::StreamExt;
use tonic::transport::Channel;

/// Files service interface.
pub struct FilesInterface {
    client: FilesClient<Channel>,
}

impl FilesInterface {
    /// Create from a channel.
    pub fn new(channel: Channel) -> Self {
        Self {
            client: FilesClient::new(channel),
        }
    }

    /// Upload a byte stream to the guest and extract at `dest_path`.
    ///
    /// `source` is the archive shape (dir tree vs single file);
    /// [`CopySourceKind::Unknown`] when the caller cannot tell — the guest then
    /// peeks the archive to decide extraction mode. It is attached to the first
    /// chunk only.
    pub async fn upload_stream<S>(
        &mut self,
        stream: S,
        dest_path: &str,
        container_id: Option<&str>,
        mkdir_parents: bool,
        overwrite: bool,
        source: CopySourceKind,
    ) -> BoxliteResult<()>
    where
        S: futures::Stream<Item = std::io::Result<Vec<u8>>> + Send + 'static,
    {
        let dest = dest_path.to_string();
        let cid = container_id.unwrap_or_default().to_string();

        // tonic client-streaming yields the message directly (no per-item
        // `Result`); a mid-stream error is signalled by ending the stream, and
        // the guest reports the failure in `UploadResponse`. The generator
        // parks the first source-stream error in a shared slot: the guest may
        // still report success on the truncated archive it received, and the
        // caller must not see that as a successful copy.
        let stream_err: std::sync::Arc<tokio::sync::Mutex<Option<std::io::Error>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let stream_err_slot = stream_err.clone();
        let source_is_dir = source.to_wire();
        let chunks = async_stream::stream! {
            futures::pin_mut!(stream);
            let mut first = true;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(data) => {
                        yield UploadChunk {
                            dest_path: if first { dest.clone() } else { String::new() },
                            container_id: cid.clone(),
                            data,
                            mkdir_parents,
                            overwrite,
                            source_is_dir: if first { source_is_dir } else { None },
                        };
                        first = false;
                    }
                    Err(e) => {
                        *stream_err_slot.lock().await = Some(e);
                        break;
                    }
                }
            }
        };

        let response = self
            .client
            .upload(chunks)
            .await
            .map_err(map_tonic_err)?
            .into_inner();

        // Prefer the source-stream failure over the guest's verdict: a pack or
        // read failure (or an explicit abort) must never surface as a
        // successful copy of a truncated archive.
        if let Some(e) = stream_err.lock().await.take() {
            return Err(BoxliteError::Internal(format!(
                "source stream failed during upload: {e}"
            )));
        }

        if response.success {
            Ok(())
        } else {
            Err(BoxliteError::Internal(
                response.error.unwrap_or_else(|| "Upload failed".into()),
            ))
        }
    }

    /// Download a path from the guest as a byte stream.
    ///
    /// Returns the stream plus the source shape read from the guest's first
    /// chunk. [`CopySourceKind::Unknown`] means the guest predates the hint
    /// (older peer).
    pub async fn download_stream(
        &mut self,
        container_src: &str,
        container_id: Option<&str>,
        include_parent: bool,
        follow_symlinks: bool,
    ) -> BoxliteResult<(BoxByteStream, CopySourceKind)> {
        let request = DownloadRequest {
            src_path: container_src.to_string(),
            container_id: container_id.unwrap_or_default().to_string(),
            include_parent,
            follow_symlinks,
        };

        let mut stream = self
            .client
            .download(request)
            .await
            .map_err(map_tonic_err)?
            .into_inner();

        let first = stream.message().await.map_err(map_tonic_err)?;
        let source = CopySourceKind::from_wire(first.as_ref().and_then(|c| c.source_is_dir));
        let mut first_data = first.map(|c| c.data);

        let out = async_stream::stream! {
            if let Some(data) = first_data.take().filter(|d| !d.is_empty()) {
                yield Ok(data);
            }
            loop {
                match stream.message().await {
                    Ok(Some(chunk)) => {
                        if !chunk.data.is_empty() {
                            yield Ok(chunk.data);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        yield Err(std::io::Error::other(e));
                        break;
                    }
                }
            }
        };

        Ok((Box::pin(out), source))
    }
}

/// Preserve the guest's error class instead of flattening it to `Internal`.
///
/// The guest rejects bad requests with real gRPC codes — an unreachable
/// destination, a missing source, a malformed path, a payload over its size
/// cap. Collapsing all of them into `Internal` turned every one into a 500 and
/// buried the reason inside a `status: …` string, which is exactly how a caller
/// ends up thinking a deliberate refusal was a server fault.
///
/// The arms are the complete set of codes `boxlite-guest`'s files service
/// emits; anything else is genuinely unclassified and stays a server fault.
fn map_tonic_err(err: tonic::Status) -> BoxliteError {
    let message = err.message().to_owned();
    match err.code() {
        tonic::Code::FailedPrecondition => BoxliteError::Unsupported(message),
        tonic::Code::InvalidArgument => BoxliteError::InvalidArgument(message),
        tonic::Code::NotFound => BoxliteError::NotFound(message),
        tonic::Code::ResourceExhausted => BoxliteError::ResourceExhausted(message),
        _ => BoxliteError::Internal(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Status;

    /// A destination the guest cannot reach is the caller's problem, not the
    /// server's — it must not arrive as a 500.
    #[test]
    fn unreachable_destination_maps_to_unsupported() {
        let error = map_tonic_err(Status::failed_precondition(
            "/tmp/x is under the container's '/tmp' mount",
        ));

        assert!(matches!(error, BoxliteError::Unsupported(_)));
        assert_eq!(error.http().0, 400);
    }

    #[test]
    fn missing_source_maps_to_not_found() {
        let error = map_tonic_err(Status::not_found("source path does not exist"));

        assert!(matches!(error, BoxliteError::NotFound(_)));
        assert_eq!(error.http().0, 404);
    }

    #[test]
    fn malformed_path_maps_to_invalid_argument() {
        let error = map_tonic_err(Status::invalid_argument("path must not contain .."));

        assert!(matches!(error, BoxliteError::InvalidArgument(_)));
        assert_eq!(error.http().0, 400);
    }

    /// The guest caps an upload at 512 MiB and says so with `ResourceExhausted`.
    /// Flattened to `Internal` it reaches the caller as a 500 — the server
    /// blaming itself for a payload the caller chose.
    #[test]
    fn oversized_upload_maps_to_resource_exhausted() {
        let error = map_tonic_err(Status::resource_exhausted("upload too large"));

        assert!(
            matches!(error, BoxliteError::ResourceExhausted(_)),
            "{error:?}"
        );
        assert_eq!(error.http().0, 429);
    }

    /// Anything the guest did not classify stays a server fault, and keeps the
    /// full status text so the code is not lost.
    #[test]
    fn unclassified_status_stays_internal() {
        let error = map_tonic_err(Status::internal("failed to create temp file"));

        assert!(matches!(error, BoxliteError::Internal(_)));
        assert_eq!(error.http().0, 500);
    }

    /// The message must survive the remap — a refusal that arrives without the
    /// mount name in it is useless to the caller.
    #[test]
    fn refusal_message_survives_the_remap() {
        let error = map_tonic_err(Status::failed_precondition(
            "/tmp/x is under the container's '/tmp' mount",
        ));

        assert!(error.to_string().contains("'/tmp' mount"), "{error}");
    }

    // ── The pre-hint guest ────────────────────────────────────────
    //
    // The guest ships inside the guest image, so it can be older than the host
    // binary that talks to it. A guest built before the archive-shape hint
    // simply leaves `source_is_dir` unset, and the host must read that as
    // `Unknown` — not as `false`, which would extract a directory archive as a
    // single file. These stand a real `Files` service up over a socket so the
    // hint crosses the wire rather than being asserted in place.

    /// A guest whose `download` sends the shape hint, or omits it the way a
    /// pre-hint peer does.
    struct StubGuest {
        source_is_dir: Option<bool>,
    }

    #[tonic::async_trait]
    impl boxlite_shared::Files for StubGuest {
        type DownloadStream =
            tokio_stream::wrappers::ReceiverStream<Result<boxlite_shared::DownloadChunk, Status>>;

        async fn upload(
            &self,
            _request: tonic::Request<tonic::Streaming<UploadChunk>>,
        ) -> Result<tonic::Response<boxlite_shared::UploadResponse>, Status> {
            Err(Status::unimplemented("this stub only answers downloads"))
        }

        async fn download(
            &self,
            _request: tonic::Request<DownloadRequest>,
        ) -> Result<tonic::Response<Self::DownloadStream>, Status> {
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let source_is_dir = self.source_is_dir;
            tokio::spawn(async move {
                // The hint rides the first chunk only — the second one proves
                // the reader keeps the first chunk's answer and its bytes.
                let _ = tx
                    .send(Ok(boxlite_shared::DownloadChunk {
                        data: b"first".to_vec(),
                        source_is_dir,
                    }))
                    .await;
                let _ = tx
                    .send(Ok(boxlite_shared::DownloadChunk {
                        data: b"second".to_vec(),
                        source_is_dir: None,
                    }))
                    .await;
            });
            Ok(tonic::Response::new(
                tokio_stream::wrappers::ReceiverStream::new(rx),
            ))
        }
    }

    async fn download_from_stub(source_is_dir: Option<bool>) -> (Vec<u8>, CopySourceKind) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = async_stream::stream! {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => yield Ok::<_, std::io::Error>(socket),
                    Err(e) => yield Err(e),
                }
            }
        };
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(boxlite_shared::FilesServer::new(StubGuest {
                    source_is_dir,
                }))
                .serve_with_incoming(incoming),
        );

        let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .expect("connect to the stub guest");
        let (mut stream, source) = FilesInterface::new(channel)
            .download_stream("/src", None, false, false)
            .await
            .expect("download");

        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
        (bytes, source)
    }

    #[tokio::test]
    async fn a_guest_that_omits_the_hint_reports_unknown() {
        let (bytes, source) = download_from_stub(None).await;

        assert_eq!(source, CopySourceKind::Unknown);
        assert_eq!(bytes, b"firstsecond");
    }

    #[tokio::test]
    async fn the_first_chunks_hint_is_the_reported_shape() {
        let (bytes, dir) = download_from_stub(Some(true)).await;
        assert_eq!(dir, CopySourceKind::Dir);
        assert_eq!(bytes, b"firstsecond");

        let (_, file) = download_from_stub(Some(false)).await;
        assert_eq!(file, CopySourceKind::File);
    }
}
