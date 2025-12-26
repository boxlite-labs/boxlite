use crate::BoxID;
use boxlite_shared::Transport;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Static box configuration (set once at creation, never changes).
///
/// This is persisted to database and remains immutable throughout the box lifecycle.
/// Follows the Podman pattern of separating config from state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxConfig {
    // === Identity & Timestamps ===
    /// Unique box identifier (ULID).
    pub id: BoxID,
    /// User-defined name (optional, must be unique if provided).
    pub name: Option<String>,
    /// Creation timestamp (UTC).
    pub created_at: DateTime<Utc>,

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
