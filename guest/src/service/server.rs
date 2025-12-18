use crate::container::Container;
use crate::layout::GuestLayout;
use crate::service::exec::registry::ExecutionRegistry;
use boxlite_shared::{BoxliteResult, Transport};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Server;
use tracing::{info, warn};

/// Guest initialization state.
///
/// Tracks the state set by Guest.Init, which must be called before Container.Init.
#[derive(Default)]
pub(crate) struct GuestInitState {
    /// Whether guest has been initialized
    pub initialized: bool,
}

/// Guest agent server.
///
/// Implements three gRPC services:
/// - Guest: Agent initialization and management
/// - Container: OCI container lifecycle
/// - Execution: Command execution with bidirectional streaming
pub(crate) struct GuestServer {
    /// Guest filesystem layout
    pub layout: GuestLayout,

    /// Guest initialization state (set by Guest.Init)
    pub init_state: Arc<Mutex<GuestInitState>>,

    /// Container registry: container_id -> Container
    pub containers: Arc<Mutex<HashMap<String, Arc<Mutex<Container>>>>>,

    /// Execution registry for tracking running executions
    pub registry: ExecutionRegistry,
}

impl GuestServer {
    /// Create a new server with the given layout.
    ///
    /// Server starts uninitialized. Guest.Init must be called first to setup
    /// the environment, then Container.Init to start the container.
    pub fn new(layout: GuestLayout) -> Self {
        Self {
            layout,
            init_state: Arc::new(Mutex::new(GuestInitState::default())),
            containers: Arc::new(Mutex::new(HashMap::new())),
            registry: ExecutionRegistry::new(),
        }
    }

    /// Run the tonic server listening on the specified transport.
    ///
    /// Binds to the specified transport (Unix, TCP, or Vsock) and serves
    /// all three gRPC services on a single port.
    ///
    /// If `notify_uri` is provided, connects to that URI after the server
    /// is ready to serve, signaling readiness to the host.
    pub async fn run(self, listen_uri: String, notify_uri: Option<String>) -> BoxliteResult<()> {
        info!("Starting tonic gRPC server");

        // Parse the listen URI to determine transport type
        let transport = Transport::from_uri(&listen_uri).map_err(|e| {
            boxlite_shared::errors::BoxliteError::Internal(format!(
                "Invalid listen URI '{}': {}",
                listen_uri, e
            ))
        })?;

        info!("Parsed transport from URI: {:?}", transport);

        // Wrap self in Arc for sharing across services
        let server = Arc::new(self);

        let server_builder = Server::builder()
            .add_service(boxlite_shared::ContainerServer::from_arc(server.clone()))
            .add_service(boxlite_shared::GuestServer::from_arc(server.clone()))
            .add_service(boxlite_shared::ExecutionServer::from_arc(server.clone()));

        match transport {
            Transport::Vsock { port } => {
                use tokio_vsock::{VsockAddr, VsockListener, VMADDR_CID_ANY};

                info!("Binding to vsock port {}", port);
                let addr = VsockAddr::new(VMADDR_CID_ANY, port);
                let listener = VsockListener::bind(addr).map_err(|e| {
                    boxlite_shared::errors::BoxliteError::Internal(format!(
                        "Failed to bind vsock: {}",
                        e
                    ))
                })?;
                info!("Listening on vsock://{}:{}", VMADDR_CID_ANY, port);

                let incoming = listener.incoming();

                tokio::spawn(async move {
                    if let Err(e) = notify_host_ready(notify_uri).await {
                        warn!("Failed to notify host: {}", e);
                    }
                });

                server_builder
                    .serve_with_incoming(incoming)
                    .await
                    .map_err(|e| {
                        boxlite_shared::errors::BoxliteError::Internal(format!(
                            "Server error: {}",
                            e
                        ))
                    })?;
            }

            Transport::Unix { socket_path } => {
                use tokio_stream::wrappers::UnixListenerStream;

                // Remove socket if it exists
                if socket_path.exists() {
                    std::fs::remove_file(&socket_path)?;
                }

                // Ensure parent directory exists
                if let Some(parent) = socket_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                info!("Binding to Unix socket: {}", socket_path.display());
                let listener = tokio::net::UnixListener::bind(&socket_path)?;
                info!("Listening on unix://{}", socket_path.display());

                let incoming = UnixListenerStream::new(listener);

                tokio::spawn(async move {
                    if let Err(e) = notify_host_ready(notify_uri).await {
                        warn!("Failed to notify host: {}", e);
                    }
                });

                server_builder
                    .serve_with_incoming(incoming)
                    .await
                    .map_err(|e| {
                        boxlite_shared::errors::BoxliteError::Internal(format!(
                            "Server error: {}",
                            e
                        ))
                    })?;
            }

            Transport::Tcp { port } => {
                use tokio_stream::wrappers::TcpListenerStream;

                let addr = format!("127.0.0.1:{}", port);
                info!("Binding to TCP address: {}", addr);
                let listener = tokio::net::TcpListener::bind(&addr).await?;
                info!("Listening on tcp://{}", addr);

                let incoming = TcpListenerStream::new(listener);

                tokio::spawn(async move {
                    if let Err(e) = notify_host_ready(notify_uri).await {
                        warn!("Failed to notify host: {}", e);
                    }
                });

                server_builder
                    .serve_with_incoming(incoming)
                    .await
                    .map_err(|e| {
                        boxlite_shared::errors::BoxliteError::Internal(format!(
                            "Server error: {}",
                            e
                        ))
                    })?;
            }
        }

        Ok(())
    }
}

/// Notify host that guest is ready by connecting to the notify URI.
///
/// The connection itself is the signal - no data needs to be sent.
async fn notify_host_ready(notify_uri: Option<String>) -> BoxliteResult<()> {
    let uri = match notify_uri {
        Some(uri) => uri,
        None => {
            info!("No notify URI provided, skipping host notification");
            return Ok(());
        }
    };

    let transport = Transport::from_uri(uri.as_str()).map_err(|e| {
        boxlite_shared::errors::BoxliteError::Internal(format!(
            "Invalid notify URI '{}': {}",
            uri, e
        ))
    })?;

    match transport {
        Transport::Vsock { port } => {
            use tokio_vsock::{VsockAddr, VsockStream, VMADDR_CID_HOST};

            info!("Notifying host via vsock:{}", port);
            let addr = VsockAddr::new(VMADDR_CID_HOST, port);
            let _stream = VsockStream::connect(addr).await.map_err(|e| {
                boxlite_shared::errors::BoxliteError::Internal(format!(
                    "Failed to connect to notify vsock: {}",
                    e
                ))
            })?;
            info!("Host notified successfully");
            // Connection itself signals readiness, drop immediately
        }
        Transport::Unix { socket_path } => {
            info!("Notifying host via unix:{}", socket_path.display());
            let _stream = tokio::net::UnixStream::connect(&socket_path)
                .await
                .map_err(|e| {
                    boxlite_shared::errors::BoxliteError::Internal(format!(
                        "Failed to connect to notify socket: {}",
                        e
                    ))
                })?;
            info!("Host notified successfully");
        }
        Transport::Tcp { port } => {
            info!("Notifying host via tcp:127.0.0.1:{}", port);
            let _stream = tokio::net::TcpStream::connect(format!("127.0.0.1:{}", port))
                .await
                .map_err(|e| {
                    boxlite_shared::errors::BoxliteError::Internal(format!(
                        "Failed to connect to notify tcp: {}",
                        e
                    ))
                })?;
            info!("Host notified successfully");
        }
    }

    Ok(())
}
