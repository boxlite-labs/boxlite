use boxlite::runtime::types::BoxInfo;
use napi_derive::napi;

/// Public metadata about a box (returned by list operations).
///
/// Provides read-only information about a box's identity, status,
/// and lifecycle timestamps.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsBoxInfo {
    /// Unique box identifier (ULID format)
    pub id: String,

    /// User-defined name (optional)
    pub name: Option<String>,

    /// Current lifecycle status (Starting, Running, etc.)
    pub status: String,

    /// Creation timestamp (ISO 8601 format)
    pub created_at: String,

    /// Last state change timestamp (ISO 8601 format)
    pub last_updated: String,

    /// Process ID of the VMM subprocess (None if not running)
    pub pid: Option<u32>,

    /// Transport mechanism for guest communication
    pub transport: String,
}

impl From<BoxInfo> for JsBoxInfo {
    fn from(info: BoxInfo) -> Self {
        Self {
            id: info.id.to_string(),
            name: info.name,
            status: info.status.to_string(),
            created_at: info.created_at.to_rfc3339(),
            last_updated: info.last_updated.to_rfc3339(),
            pid: info.pid,
            transport: info.transport.to_string(),
        }
    }
}
