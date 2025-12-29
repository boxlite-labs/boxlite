//! Core data types for box lifecycle management.

use chrono::{DateTime, Utc};
use rand::RngCore;
use rusqlite::ToSql;
use rusqlite::types::{ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

use boxlite_shared::Transport;

// Re-export status types from litebox module
pub use crate::litebox::{BoxState, BoxStatus};

// ============================================================================
// BOX ID
// ============================================================================

/// Box identifier (ULID format for sortability).
///
/// ULIDs are 26-character strings that encode:
/// - 48-bit timestamp (millisecond precision)
/// - 80 bits of randomness
/// - Lexicographically sortable by creation time
///
/// # Example
///
/// ```
/// use boxlite::runtime::types::BoxID;
///
/// let id = BoxID::new();
/// assert_eq!(id.as_str().len(), 26);
/// assert_eq!(id.short().len(), 8);
/// ```
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BoxID(String);

impl BoxID {
    /// Length of full box ID (26 chars = ULID format).
    pub const FULL_LENGTH: usize = 26;

    /// Length of short box ID for display (8 chars).
    pub const SHORT_LENGTH: usize = 8;

    /// Generate a new ULID-based box ID.
    pub fn new() -> Self {
        Self(ulid::Ulid::new().to_string())
    }

    /// Parse a BoxID from an existing string.
    ///
    /// Returns `None` if the string is not a valid 26-char ULID string.
    pub fn parse(s: &str) -> Option<Self> {
        if Self::is_valid(s) {
            Some(Self(s.to_string()))
        } else {
            None
        }
    }

    /// Check if a string is a valid box ID format.
    pub fn is_valid(s: &str) -> bool {
        s.len() == Self::FULL_LENGTH && ulid::Ulid::from_string(s).is_ok()
    }

    /// Get the full box ID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Get the short form (first 8 characters) for display.
    pub fn short(&self) -> &str {
        &self.0[..Self::SHORT_LENGTH]
    }

    /// Check if this ID starts with the given prefix.
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.0.starts_with(prefix)
    }
}

impl Default for BoxID {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BoxID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for BoxID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BoxID({})", self.short())
    }
}

impl AsRef<str> for BoxID {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for BoxID {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl ToSql for BoxID {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0.as_bytes())))
    }
}

// ============================================================================
// CONTAINER ID
// ============================================================================

/// Container identifier (64-character lowercase hex).
///
/// Follows the OCI convention: SHA256 hash encoded as 64 lowercase hex characters.
/// This format matches Docker/containerd container IDs.
///
/// # Example
///
/// ```
/// use boxlite::runtime::types::ContainerID;
///
/// let id = ContainerID::new();
/// assert_eq!(id.as_str().len(), 64);
/// assert_eq!(id.short().len(), 12);
/// ```
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerID(String);

impl ContainerID {
    /// Length of full container ID (64 hex chars = 256 bits).
    pub const FULL_LENGTH: usize = 64;

    /// Length of short container ID for display (12 hex chars).
    pub const SHORT_LENGTH: usize = 12;

    /// Generate a new random container ID.
    ///
    /// Uses SHA256 of 32 random bytes to produce a 64-char hex string.
    pub fn new() -> Self {
        let mut random_bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut random_bytes);

        let mut hasher = Sha256::new();
        hasher.update(random_bytes);
        let result = hasher.finalize();

        Self(hex::encode(result))
    }

    /// Parse a ContainerID from an existing string.
    ///
    /// Returns `None` if the string is not a valid 64-char lowercase hex string.
    pub fn parse(s: &str) -> Option<Self> {
        if Self::is_valid(s) {
            Some(Self(s.to_string()))
        } else {
            None
        }
    }

    /// Check if a string is a valid container ID format.
    pub fn is_valid(s: &str) -> bool {
        s.len() == Self::FULL_LENGTH
            && s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
    }

    /// Get the full container ID as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Get the short form (first 12 characters) for display.
    pub fn short(&self) -> &str {
        &self.0[..Self::SHORT_LENGTH]
    }
}

impl Default for ContainerID {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ContainerID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for ContainerID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContainerID({})", self.short())
    }
}

impl AsRef<str> for ContainerID {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Public metadata about a box (returned by list operations).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxInfo {
    /// Unique box identifier (ULID).
    pub id: BoxID,

    /// User-defined name (optional).
    pub name: Option<String>,

    /// Current lifecycle status.
    pub status: BoxStatus,

    /// Creation timestamp (UTC).
    pub created_at: DateTime<Utc>,

    /// Last state change timestamp (UTC).
    pub last_updated: DateTime<Utc>,

    /// Process ID of the VMM subprocess (None if not running).
    pub pid: Option<u32>,

    /// Transport mechanism for guest communication.
    pub transport: Transport,

    /// Image reference or rootfs path.
    pub image: String,

    /// Allocated CPU count.
    pub cpus: u8,

    /// Allocated memory in MiB.
    pub memory_mib: u32,

    /// User-defined labels for filtering and organization.
    pub labels: HashMap<String, String>,
}

impl BoxInfo {
    /// Create BoxInfo from config and state.
    pub fn new(config: &crate::litebox::config::BoxConfig, state: &BoxState) -> Self {
        use crate::runtime::options::RootfsSpec;

        Self {
            id: config.id.clone(),
            name: config.name.clone(),
            status: state.status,
            created_at: config.created_at,
            last_updated: state.last_updated,
            pid: state.pid,
            transport: config.transport.clone(),
            image: match &config.options.rootfs {
                RootfsSpec::Image(r) => r.clone(),
                RootfsSpec::RootfsPath(p) => format!("rootfs:{}", p),
            },
            cpus: config.options.cpus.unwrap_or(2),
            memory_mib: config.options.memory_mib.unwrap_or(512),
            labels: HashMap::new(),
        }
    }
}

impl PartialEq for BoxInfo {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.status == other.status
            && self.created_at == other.created_at
            && self.pid == other.pid
            && self.image == other.image
            && self.cpus == other.cpus
            && self.memory_mib == other.memory_mib
            && self.labels == other.labels
    }
}

// ============================================================================
// BOX CONFIG (Podman-style separation)
// ============================================================================

// BoxMetadata is replaced by BoxConfig + BoxState
// Old BoxMetadata struct removed - use BoxConfig + BoxState instead

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litebox::config::{BoxConfig, ContainerRuntimeConfig};
    use crate::runtime::options::{BoxOptions, RootfsSpec};
    use std::path::PathBuf;

    #[test]
    fn test_box_id_new() {
        let id1 = BoxID::new();
        let id2 = BoxID::new();

        // IDs should be 26 characters (ULID format)
        assert_eq!(id1.as_str().len(), BoxID::FULL_LENGTH);
        assert_eq!(id2.as_str().len(), BoxID::FULL_LENGTH);

        // IDs should be unique
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_box_id_short() {
        let id = BoxID::new();

        // Short form should be 8 characters
        assert_eq!(id.short().len(), BoxID::SHORT_LENGTH);

        // Short form should be prefix of full ID
        assert!(id.as_str().starts_with(id.short()));
    }

    #[test]
    fn test_box_id_parse() {
        // Valid ULID
        let valid = "01HJK4TNRPQSXYZ8WM6NCVT9R5";
        assert!(BoxID::parse(valid).is_some());

        // Invalid: too short
        assert!(BoxID::parse("abc123").is_none());

        // Invalid: wrong length
        assert!(BoxID::parse("01HJK4TNRPQSXYZ8WM6NCVT9R5X").is_none());
    }

    #[test]
    fn test_box_id_display() {
        let id = BoxID::new();
        let display = format!("{}", id);
        assert_eq!(display, id.as_str());
    }

    #[test]
    fn test_box_id_debug() {
        let id = BoxID::new();
        let debug = format!("{:?}", id);
        assert!(debug.contains(id.short()));
        assert!(debug.starts_with("BoxID("));
    }

    // BoxStatus and BoxState tests are in litebox/state

    #[test]
    fn test_config_state_to_info() {
        let now = Utc::now();
        let box_id = BoxID::parse("01HJK4TNRPQSXYZ8WM6NCVT9R5").unwrap();
        let config = BoxConfig {
            id: box_id,
            name: None,
            created_at: now,
            container: ContainerRuntimeConfig {
                id: ContainerID::new(),
            },
            options: BoxOptions {
                rootfs: RootfsSpec::Image("python:3.11".to_string()),
                cpus: Some(4),
                memory_mib: Some(1024),
                ..Default::default()
            },
            engine_kind: crate::vmm::VmmKind::Libkrun,
            transport: Transport::unix(PathBuf::from("/tmp/boxlite.sock")),
            box_home: PathBuf::from("/tmp/box"),
            ready_socket_path: PathBuf::from("/tmp/ready.sock"),
        };

        let mut state = BoxState::new();
        state.set_pid(Some(12345));
        let _ = state.transition_to(BoxStatus::Running);

        let info = BoxInfo::new(&config, &state);

        assert_eq!(info.id, config.id);
        assert_eq!(info.status, state.status);
        assert_eq!(info.created_at, config.created_at);
        assert_eq!(info.pid, state.pid);
        assert_eq!(info.transport, config.transport);
        assert_eq!(info.image, "python:3.11");
        assert_eq!(info.cpus, 4);
        assert_eq!(info.memory_mib, 1024);
    }

    #[test]
    fn test_container_id_new() {
        let id1 = ContainerID::new();
        let id2 = ContainerID::new();

        // IDs should be 64 characters
        assert_eq!(id1.as_str().len(), ContainerID::FULL_LENGTH);
        assert_eq!(id2.as_str().len(), ContainerID::FULL_LENGTH);

        // IDs should be unique
        assert_ne!(id1, id2);

        // IDs should be lowercase hex
        assert!(
            id1.as_str()
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }

    #[test]
    fn test_container_id_short() {
        let id = ContainerID::new();

        // Short form should be 12 characters
        assert_eq!(id.short().len(), ContainerID::SHORT_LENGTH);

        // Short form should be prefix of full ID
        assert!(id.as_str().starts_with(id.short()));
    }

    #[test]
    fn test_container_id_from_str() {
        // Valid ID
        let valid = "a".repeat(64);
        assert!(ContainerID::parse(&valid).is_some());

        // Invalid: too short
        assert!(ContainerID::parse("abc123").is_none());

        // Invalid: uppercase
        let uppercase = "A".repeat(64);
        assert!(ContainerID::parse(&uppercase).is_none());

        // Invalid: non-hex
        let non_hex = "g".repeat(64);
        assert!(ContainerID::parse(&non_hex).is_none());
    }

    #[test]
    fn test_container_id_display() {
        let id = ContainerID::new();
        let display = format!("{}", id);
        assert_eq!(display, id.as_str());
    }

    #[test]
    fn test_container_id_debug() {
        let id = ContainerID::new();
        let debug = format!("{:?}", id);
        assert!(debug.contains(id.short()));
        assert!(debug.starts_with("ContainerID("));
    }
}
