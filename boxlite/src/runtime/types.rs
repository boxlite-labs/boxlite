//! Core data types for box lifecycle management.

use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;

use boxlite_shared::Transport;

/// Box identifier (ULID format for sortability).
///
/// ULIDs are 26-character strings that encode:
/// - 48-bit timestamp (millisecond precision)
/// - 80 bits of randomness
/// - Lexicographically sortable by creation time
///
/// Example: `01HJK4TNRPQSXYZ8WM6NCVT9R5`
pub type BoxID = String;

/// Generate a new ULID-based box ID.
pub fn generate_box_id() -> BoxID {
    ulid::Ulid::new().to_string()
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
/// use boxlite::runtime::types::ContainerId;
///
/// let id = ContainerId::new();
/// assert_eq!(id.as_str().len(), 64);
/// assert_eq!(id.short().len(), 12);
/// ```
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContainerId(String);

impl ContainerId {
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

    /// Parse a ContainerId from an existing string.
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

impl Default for ContainerId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContainerId({})", self.short())
    }
}

impl AsRef<str> for ContainerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Lifecycle state of a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoxState {
    /// Box subprocess is spawned but guest is not yet ready.
    Starting,

    /// Box is running and the guest server is accepting commands.
    Running,

    /// Box was shut down gracefully via `shutdown()`.
    Stopped,

    /// Box crashed, failed to start, or initialization timed out.
    Failed,
}

impl BoxState {
    /// Check if this state represents an active box.
    pub fn is_active(&self) -> bool {
        matches!(self, BoxState::Starting | BoxState::Running)
    }

    /// Check if this state represents a terminal state (no longer active).
    pub fn is_terminal(&self) -> bool {
        matches!(self, BoxState::Stopped | BoxState::Failed)
    }
}

/// Public metadata about a box (returned by list operations).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxInfo {
    /// Unique box identifier (ULID).
    pub id: BoxID,

    /// Current lifecycle state.
    pub state: BoxState,

    /// Creation timestamp (UTC).
    pub created_at: DateTime<Utc>,

    /// Process ID of the boxlite-shim subprocess (None if not started yet).
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

/// Internal metadata stored in the manager.
///
/// This contains all information needed to track a box,
/// including fields not exposed in the public API.
#[derive(Debug, Clone)]
#[allow(dead_code)] // engine_kind may be used in future phases
pub(crate) struct BoxMetadata {
    pub id: BoxID,
    pub state: BoxState,
    pub created_at: DateTime<Utc>,
    pub pid: Option<u32>,
    pub transport: Transport,

    // Original options used to create the box
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub labels: HashMap<String, String>,

    // Internal tracking
    pub engine_kind: crate::vmm::VmmKind,
}

impl BoxMetadata {
    /// Convert internal metadata to public BoxInfo.
    pub fn to_info(&self) -> BoxInfo {
        BoxInfo {
            id: self.id.clone(),
            state: self.state,
            created_at: self.created_at,
            pid: self.pid,
            transport: self.transport.clone(),
            image: self.image.clone(),
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            labels: self.labels.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_box_id() {
        let id1 = generate_box_id();
        let id2 = generate_box_id();

        // IDs should be 26 characters (ULID format)
        assert_eq!(id1.len(), 26);
        assert_eq!(id2.len(), 26);

        // IDs should be unique
        assert_ne!(id1, id2);

        // IDs should be sortable (later ID > earlier ID)
        assert!(id2 > id1);
    }

    #[test]
    fn test_box_state_is_active() {
        assert!(BoxState::Starting.is_active());
        assert!(BoxState::Running.is_active());
        assert!(!BoxState::Stopped.is_active());
        assert!(!BoxState::Failed.is_active());
    }

    #[test]
    fn test_box_state_is_terminal() {
        assert!(!BoxState::Starting.is_terminal());
        assert!(!BoxState::Running.is_terminal());
        assert!(BoxState::Stopped.is_terminal());
        assert!(BoxState::Failed.is_terminal());
    }

    #[test]
    fn test_metadata_to_info() {
        use std::path::PathBuf;

        let metadata = BoxMetadata {
            id: "01HJK4TNRPQSXYZ8WM6NCVT9R5".to_string(),
            state: BoxState::Running,
            created_at: Utc::now(),
            pid: Some(12345),
            transport: Transport::unix(PathBuf::from("/tmp/boxlite.sock")),
            image: "python:3.11".to_string(),
            cpus: 4,
            memory_mib: 1024,
            labels: HashMap::new(),
            engine_kind: crate::vmm::VmmKind::Libkrun,
        };

        let info = metadata.to_info();

        assert_eq!(info.id, metadata.id);
        assert_eq!(info.state, metadata.state);
        assert_eq!(info.pid, metadata.pid);
        assert_eq!(info.transport, metadata.transport);
        assert_eq!(info.image, metadata.image);
        assert_eq!(info.cpus, metadata.cpus);
        assert_eq!(info.memory_mib, metadata.memory_mib);
    }

    #[test]
    fn test_container_id_new() {
        let id1 = ContainerId::new();
        let id2 = ContainerId::new();

        // IDs should be 64 characters
        assert_eq!(id1.as_str().len(), ContainerId::FULL_LENGTH);
        assert_eq!(id2.as_str().len(), ContainerId::FULL_LENGTH);

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
        let id = ContainerId::new();

        // Short form should be 12 characters
        assert_eq!(id.short().len(), ContainerId::SHORT_LENGTH);

        // Short form should be prefix of full ID
        assert!(id.as_str().starts_with(id.short()));
    }

    #[test]
    fn test_container_id_from_str() {
        // Valid ID
        let valid = "a".repeat(64);
        assert!(ContainerId::parse(&valid).is_some());

        // Invalid: too short
        assert!(ContainerId::parse("abc123").is_none());

        // Invalid: uppercase
        let uppercase = "A".repeat(64);
        assert!(ContainerId::parse(&uppercase).is_none());

        // Invalid: non-hex
        let non_hex = "g".repeat(64);
        assert!(ContainerId::parse(&non_hex).is_none());
    }

    #[test]
    fn test_container_id_display() {
        let id = ContainerId::new();
        let display = format!("{}", id);
        assert_eq!(display, id.as_str());
    }

    #[test]
    fn test_container_id_debug() {
        let id = ContainerId::new();
        let debug = format!("{:?}", id);
        assert!(debug.contains(id.short()));
        assert!(debug.starts_with("ContainerId("));
    }
}
