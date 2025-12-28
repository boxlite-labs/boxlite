use crate::BoxID;
use crate::images::ContainerImageConfig;
use crate::runtime::options::RootfsSpec;
use crate::runtime::types::ContainerId;
use boxlite_shared::Transport;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Container runtime configuration.
///
/// Holds the container's identity, image reference, and extracted image config.
/// Owned by BoxConfig since each box runs exactly one container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerRuntimeConfig {
    /// Container ID (64-char hex, generated at box creation).
    pub id: ContainerId,
    /// Image reference or rootfs path.
    pub image: RootfsSpec,
    /// Container image config extracted from OCI image.
    /// None until image is pulled during initialization.
    pub image_config: Option<ContainerImageConfig>,
}

/// Static box configuration (set once at creation, never changes).
///
/// This is persisted to database and remains immutable throughout the box lifecycle.
/// Separates static configuration from dynamic state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxConfig {
    // === Identity & Timestamps ===
    /// Unique box identifier (ULID).
    pub id: BoxID,
    /// User-defined name (optional, must be unique if provided).
    pub name: Option<String>,
    /// Creation timestamp (UTC).
    pub created_at: DateTime<Utc>,

    // === Container Configuration ===
    /// Container configuration (id, image, image_config).
    pub container: ContainerRuntimeConfig,

    // === User Options (preserved for restart) ===
    /// User-provided options at creation time.
    /// These are preserved to allow proper restart with the same configuration.
    pub options: crate::runtime::options::BoxOptions,

    // === Runtime-Generated Configuration ===
    /// VMM engine type.
    pub engine_kind: crate::vmm::VmmKind,
    /// Transport mechanism for guest communication.
    pub transport: Transport,
    /// Box home directory.
    pub box_home: PathBuf,
    /// Ready signal socket path.
    pub ready_socket_path: PathBuf,
}
