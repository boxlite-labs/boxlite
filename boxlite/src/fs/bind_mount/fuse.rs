//! FUSE-based bind mount for rootless operation.
//!
//! Uses fuse-backend-rs passthrough filesystem with fusermount3.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::passthrough::{Config, PassthroughFs};
use fuse_backend_rs::transport::{FuseChannel, FuseSession};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use tracing::{debug, error, warn};

use super::{BindMountConfig, BindMountImpl, ensure_target_dir_exists};

pub struct FuseBindMount {
    target: PathBuf,
    session: Option<FuseSession>,
    server_thread: Option<JoinHandle<()>>,
    mounted: bool,
}

impl FuseBindMount {
    pub fn create(config: &BindMountConfig) -> BoxliteResult<Self> {
        let source = config.source.to_path_buf();
        let target = config.target.to_path_buf();

        ensure_target_dir_exists(&target)?;

        let fs = create_passthrough_fs(&source, config.read_only)?;
        let mut session = create_fuse_session(&target)?;
        mount_session(&mut session, &target)?;

        let channel = create_channel(&session)?;
        let server_thread = spawn_server_thread(fs, channel);

        debug!(
            source = %source.display(),
            target = %target.display(),
            read_only = config.read_only,
            "FUSE bind mount created"
        );

        Ok(Self {
            target,
            session: Some(session),
            server_thread: Some(server_thread),
            mounted: true,
        })
    }
}

impl BindMountImpl for FuseBindMount {
    fn target(&self) -> &Path {
        &self.target
    }

    fn unmount(&mut self) -> BoxliteResult<()> {
        if !self.mounted {
            return Ok(());
        }
        self.mounted = false;

        // Unmount session first
        if let Some(mut session) = self.session.take() {
            let _ = session.wake();
            session.umount().map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to unmount FUSE bind mount {}: {}",
                    self.target.display(),
                    e
                ))
            })?;
        }

        // Wait for server thread
        if let Some(thread) = self.server_thread.take() {
            thread.join().ok();
        }

        debug!(target = %self.target.display(), "FUSE bind mount unmounted");
        Ok(())
    }
}

// ============================================================================
// Helper functions
// ============================================================================

fn create_passthrough_fs(source: &Path, read_only: bool) -> BoxliteResult<Arc<PassthroughFs>> {
    let config = Config {
        root_dir: source.to_string_lossy().to_string(),
        do_import: false,
        writeback: !read_only,
        no_open: true,
        no_opendir: true,
        killpriv_v2: false,
        ..Default::default()
    };

    let fs = PassthroughFs::new(config).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create passthrough filesystem for {}: {}",
            source.display(),
            e
        ))
    })?;

    fs.import().map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to initialize passthrough filesystem for {}: {}",
            source.display(),
            e
        ))
    })?;

    Ok(Arc::new(fs))
}

fn create_fuse_session(target: &Path) -> BoxliteResult<FuseSession> {
    FuseSession::new(target, "boxlite-bindfs", "", true).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create FUSE session for {}: {}",
            target.display(),
            e
        ))
    })
}

fn mount_session(session: &mut FuseSession, target: &Path) -> BoxliteResult<()> {
    session.mount().map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to mount FUSE filesystem at {}: {}",
            target.display(),
            e
        ))
    })
}

fn create_channel(session: &FuseSession) -> BoxliteResult<FuseChannel> {
    session
        .new_channel()
        .map_err(|e| BoxliteError::Storage(format!("Failed to create FUSE channel: {}", e)))
}

fn spawn_server_thread(fs: Arc<PassthroughFs>, channel: FuseChannel) -> JoinHandle<()> {
    std::thread::spawn(move || {
        serve_requests(fs, channel);
    })
}

fn serve_requests(fs: Arc<PassthroughFs>, mut channel: FuseChannel) {
    let server = Server::new(fs);

    loop {
        match channel.get_request() {
            Ok(Some((reader, writer))) => {
                if let Err(e) = server.handle_message(reader, writer.into(), None, None) {
                    match e {
                        fuse_backend_rs::Error::EncodeMessage(ref io_err)
                            if is_io_channel_closed(io_err) =>
                        {
                            break;
                        }
                        _ => warn!("FUSE message handling error: {}", e),
                    }
                }
            }
            Ok(None) => break,
            Err(e) => {
                if is_transport_channel_closed(&e) {
                    break;
                }
                error!("FUSE channel error: {}", e);
                break;
            }
        }
    }
}

fn is_io_channel_closed(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EBADF) | Some(libc::ENODEV))
}

fn is_transport_channel_closed(e: &fuse_backend_rs::transport::Error) -> bool {
    match e {
        fuse_backend_rs::transport::Error::IoError(io_err) => is_io_channel_closed(io_err),
        _ => false,
    }
}
