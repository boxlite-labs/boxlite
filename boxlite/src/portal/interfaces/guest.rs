//! Guest service interface.

use boxlite_shared::{
    BlockDeviceSource, BoxliteError, BoxliteResult, Filesystem, GuestClient, GuestInitRequest,
    NetworkInit, PingRequest, ShutdownRequest, VirtiofsSource, Volume, guest_init_response,
};
use tonic::transport::Channel;

/// Guest service interface.
pub struct GuestInterface {
    client: GuestClient<Channel>,
}

impl GuestInterface {
    /// Create from a channel.
    pub fn new(channel: Channel) -> Self {
        Self {
            client: GuestClient::new(channel),
        }
    }

    /// Initialize guest environment.
    ///
    /// This must be called first after connection, before Container.Init.
    /// Sets up volumes (virtiofs + block devices) and network.
    pub async fn init(&mut self, config: GuestInitConfig) -> BoxliteResult<()> {
        tracing::debug!("Sending GuestInit request");
        tracing::trace!(
            volumes = config.volumes.len(),
            network = ?config.network,
            "Guest init configuration"
        );

        let request = GuestInitRequest {
            volumes: config.volumes.into_iter().map(|v| v.into_proto()).collect(),
            network: config.network.map(|n| NetworkInit {
                interface: n.interface,
                ip: n.ip,
                gateway: n.gateway,
            }),
        };

        let response = self.client.init(request).await?.into_inner();

        match response.result {
            Some(guest_init_response::Result::Success(_)) => {
                tracing::debug!("Guest initialized");
                Ok(())
            }
            Some(guest_init_response::Result::Error(err)) => {
                tracing::error!("Guest init failed: {}", err.reason);
                Err(BoxliteError::Internal(format!(
                    "Guest init failed: {}",
                    err.reason
                )))
            }
            None => Err(BoxliteError::Internal(
                "GuestInit response missing result".to_string(),
            )),
        }
    }

    /// Ping the guest (health check).
    #[allow(dead_code)] // API method for future health checks
    pub async fn ping(&mut self) -> BoxliteResult<()> {
        let _response = self.client.ping(PingRequest {}).await?;
        Ok(())
    }

    /// Shutdown the guest agent.
    pub async fn shutdown(&mut self) -> BoxliteResult<()> {
        let _response = self.client.shutdown(ShutdownRequest {}).await?;
        Ok(())
    }
}

/// Configuration for guest initialization.
#[derive(Debug)]
pub struct GuestInitConfig {
    /// Volumes to mount (virtiofs + block devices)
    pub volumes: Vec<VolumeConfig>,
    /// Network configuration (optional)
    pub network: Option<NetworkInitConfig>,
}

/// Volume configuration.
#[derive(Debug, Clone)]
pub enum VolumeConfig {
    /// Virtiofs mount
    Virtiofs {
        /// Virtiofs tag
        tag: String,
        /// Mount point in guest
        mount_point: String,
        read_only: bool,
        /// Optional container_id for convention-based paths
        container_id: Option<String>,
    },
    /// Block device mount
    BlockDevice {
        /// Device path (e.g., "/dev/vda")
        device: String,
        /// Mount point in guest
        mount_point: String,
        /// Filesystem type (UNSPECIFIED = don't format, use existing)
        filesystem: Filesystem,
    },
}

impl VolumeConfig {
    /// Create virtiofs volume config.
    pub fn virtiofs(
        tag: impl Into<String>,
        mount_point: impl Into<String>,
        read_only: bool,
        container_id: Option<String>,
    ) -> Self {
        Self::Virtiofs {
            tag: tag.into(),
            mount_point: mount_point.into(),
            read_only,
            container_id,
        }
    }

    /// Create block device volume config.
    pub fn block_device(
        device: impl Into<String>,
        mount_point: impl Into<String>,
        filesystem: Filesystem,
    ) -> Self {
        Self::BlockDevice {
            device: device.into(),
            mount_point: mount_point.into(),
            filesystem,
        }
    }

    fn into_proto(self) -> Volume {
        match self {
            VolumeConfig::Virtiofs {
                tag,
                mount_point,
                read_only,
                container_id,
            } => Volume {
                mount_point,
                source: Some(boxlite_shared::volume::Source::Virtiofs(VirtiofsSource {
                    tag,
                    read_only,
                })),
                container_id: container_id.unwrap_or_default(),
            },
            VolumeConfig::BlockDevice {
                device,
                mount_point,
                filesystem,
            } => Volume {
                mount_point,
                source: Some(boxlite_shared::volume::Source::BlockDevice(
                    BlockDeviceSource {
                        device,
                        filesystem: filesystem.into(),
                    },
                )),
                container_id: String::new(),
            },
        }
    }
}

/// Network initialization configuration.
#[derive(Debug)]
pub struct NetworkInitConfig {
    /// Interface name (e.g., "eth0")
    pub interface: String,
    /// IP address with prefix (e.g., "192.168.127.2/24")
    pub ip: Option<String>,
    /// Gateway address (e.g., "192.168.127.1")
    pub gateway: Option<String>,
}
