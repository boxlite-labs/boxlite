//! Wire types (request/response JSON) for the REST API.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Box Types
// ============================================================================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateBoxRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub rootfs_path: Option<String>,
    #[serde(default)]
    pub cpus: Option<u8>,
    #[serde(default)]
    pub memory_mib: Option<u32>,
    #[serde(default)]
    pub disk_size_gb: Option<u64>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    #[serde(default)]
    pub cmd: Option<Vec<String>>,
    #[serde(default)]
    pub user: Option<String>,
    /// Run the box's main command on a terminal (docker `run -t`). Belongs on
    /// *create* because the main command is the container's init: whether it
    /// gets a PTY is fixed when the container is built, and no later attach can
    /// add one.
    #[serde(default)]
    pub tty: Option<bool>,
    /// Expert-only container options.
    #[serde(default)]
    pub advanced: CreateBoxAdvancedOptions,
    #[serde(default)]
    pub network: Option<NetworkSpec>,
    #[serde(default)]
    pub auto_stop: Option<u32>,
    #[serde(default)]
    pub auto_delete: Option<u32>,
    #[serde(default)]
    pub auto_resume: Option<bool>,
    #[serde(default)]
    pub detach: Option<bool>,
    /// Secret placeholder rules for outbound HTTP(S) requests. Placeholder
    /// defaults to `<BOXLITE_SECRET:{name}>` when omitted — applied in
    /// `build_box_options`, matching the Go SDK's own default.
    #[serde(default)]
    pub secrets: Option<Vec<SecretSpecRequest>>,
    /// Managed volumes are not supported by `boxlite serve` (no volume
    /// backend). Accepted here only so a combined secrets+volumes request does
    /// not fail deserialization on an unrelated field; rejected explicitly in
    /// `build_box_options` when non-empty.
    #[serde(default)]
    pub volumes: Option<Vec<serde_json::Value>>,
    // `security` / `security_settings` are intentionally absent from
    // the REST wire schema. Sandbox security is the operator's
    // policy, set server-side. Because the struct carries
    // `#[serde(deny_unknown_fields)]`, any attempt by a client to
    // smuggle a security knob in surfaces as a 400 from
    // serde_json::from_str — there is no quiet fall-through. See
    // `build_box_options_rejects_client_supplied_security_*` tests
    // below for the wire-shape pin.
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SecretSpecRequest {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub hosts: Vec<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct CreateBoxAdvancedOptions {
    /// `None` when the wire omits `capabilities` (or sends `null`) — distinct
    /// from `Some` of an empty policy, the same distinction the core
    /// `AdvancedBoxOptions.capabilities` makes and for the same reason: an
    /// explicit empty policy conflicts with `privileged`, an unspecified one
    /// doesn't, and `archive_version_for_options` keys off which one this is.
    pub capabilities: Option<ContainerCapabilitiesRequest>,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ContainerCapabilitiesRequest {
    pub add: Vec<String>,
    pub drop: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NetworkSpec {
    #[serde(default)]
    pub outbound: Option<OutboundNetworkSpec>,
    #[serde(default)]
    pub inbound: Option<InboundNetworkSpec>,
    #[serde(flatten)]
    pub legacy: LegacyNetworkSpec,
}

impl NetworkSpec {
    pub(super) fn uses_legacy_fields(&self) -> bool {
        self.legacy.mode.is_some() || self.legacy.allow_net.is_some()
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyNetworkSpec {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub allow_net: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OutboundNetworkSpec {
    pub mode: String,
    #[serde(default)]
    pub allow_net: Vec<String>,
}

/// Aligned field-for-field with [`OutboundNetworkSpec`]: `mode="enabled"`
/// means services the box exposes are publicly reachable, `mode="disabled"`
/// means private. `allow_net` exists for shape symmetry only — no layer
/// enforces an inbound allowlist yet, so a non-empty value is rejected.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InboundNetworkSpec {
    pub mode: String,
    #[serde(default)]
    pub allow_net: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct BoxResponse {
    pub box_id: String,
    pub name: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    pub pid: Option<u32>,
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u32,
    pub labels: HashMap<String, String>,
    pub auto_stop: u32,
    pub auto_delete: u32,
    pub auto_resume: bool,
    /// The status the box's main command exited with, once it has. `None`
    /// while it is still running — a remote `inspect` must be able to tell
    /// "not finished" apart from "finished with 0".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

#[derive(Serialize)]
pub(super) struct ListBoxesResponse {
    pub boxes: Vec<BoxResponse>,
}

// ============================================================================
// Named volume types (`/v1/volumes`)
// ============================================================================

#[derive(Serialize)]
pub(super) struct VolumeResponse {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub size_bytes: Option<u64>,
}

/// Body for `POST /v1/volumes`. An absent body means an unnamed volume.
#[derive(Deserialize, Default)]
pub(super) struct CreateVolumeRequest {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Serialize)]
pub(super) struct ListVolumesResponse {
    pub volumes: Vec<VolumeResponse>,
}

// ============================================================================
// Execution Types
// ============================================================================

#[derive(Deserialize)]
pub(super) struct ExecRequest {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub timeout_seconds: Option<f64>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub tty: bool,
}

#[derive(Serialize)]
pub(super) struct ExecResponse {
    pub execution_id: String,
}

#[derive(Deserialize)]
pub(super) struct SignalRequest {
    pub signal: i32,
}

#[derive(Deserialize)]
pub(super) struct ResizeRequest {
    pub cols: u32,
    pub rows: u32,
}

// ============================================================================
// Config Types
// ============================================================================

/// Server configuration & capabilities — the `GET /v1/config` response
/// from the local Axum reference server. Mirrors the `ServerConfig`
/// schema in `openapi/box.openapi.yaml`.
#[derive(Serialize)]
pub(super) struct ServerConfig {
    pub capabilities: ServerCapabilities,
}

#[derive(Serialize)]
pub(super) struct ServerCapabilities {
    pub linux_capabilities_enabled: bool,
    pub snapshots_enabled: bool,
    pub clone_enabled: bool,
    pub export_enabled: bool,
    pub import_enabled: bool,
}

// ============================================================================
// Snapshot Types
// ============================================================================

#[derive(Deserialize)]
pub(super) struct CreateSnapshotRequest {
    pub name: String,
}

#[derive(Serialize)]
pub(super) struct SnapshotResponse {
    pub id: String,
    pub box_id: String,
    pub name: String,
    pub created_at: i64,
    pub container_disk_bytes: u64,
    pub size_bytes: u64,
}

#[derive(Serialize)]
pub(super) struct ListSnapshotsResponse {
    pub snapshots: Vec<SnapshotResponse>,
}

// ============================================================================
// Clone & Import Types
// ============================================================================

#[derive(Deserialize)]
pub(super) struct CloneRequest {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImportQuery {
    #[serde(default)]
    pub name: Option<String>,
}

// ============================================================================
// Metrics Types
// ============================================================================

#[derive(Serialize)]
pub(super) struct RuntimeMetricsResponse {
    pub boxes_created_total: u64,
    pub boxes_failed_total: u64,
    pub boxes_stopped_total: u64,
    pub num_running_boxes: u64,
    pub total_commands_executed: u64,
    pub total_exec_errors: u64,
}

#[derive(Serialize)]
pub(super) struct BoxMetricsResponse {
    pub commands_executed_total: u64,
    pub exec_errors_total: u64,
    pub bytes_sent_total: u64,
    pub bytes_received_total: u64,
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub network_bytes_sent: Option<u64>,
    pub network_bytes_received: Option<u64>,
    pub network_tcp_connections: Option<u64>,
    pub network_tcp_errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boot_timing: Option<BootTimingResponse>,
}

#[derive(Serialize)]
pub(super) struct BootTimingResponse {
    pub total_create_ms: Option<u64>,
    pub guest_boot_ms: Option<u64>,
    pub filesystem_setup_ms: Option<u64>,
    pub image_prepare_ms: Option<u64>,
    pub guest_rootfs_ms: Option<u64>,
    pub box_config_ms: Option<u64>,
    pub box_spawn_ms: Option<u64>,
    pub container_init_ms: Option<u64>,
}

// ============================================================================
// Error Types
// ============================================================================

#[derive(Serialize)]
pub(super) struct ErrorBody {
    pub error: ErrorDetail,
}

/// Wire shape for HTTP error responses.
///
/// - `message` — human-readable description with context (for logs and
///   end-user display).
/// - `type` — stable PascalCase identifier (K8s `Status.reason` style).
///   `BoxliteError::http()` is the source of truth in
///   `boxlite_shared::errors`.
/// - `code` — stable snake_case machine identifier (Stripe `code` style).
///   Clients pattern-match on this for typed error handling.
/// - `request_id` — populated from `X-Request-Id` if propagated by the
///   middleware; omitted when absent.
///
/// The HTTP numeric status lives in the response status line, not in the
/// body — including it twice (header + body) was the legacy shape and
/// added no information.
#[derive(Serialize)]
pub(super) struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

// ============================================================================
// Query Types
// ============================================================================

#[derive(Deserialize)]
pub(super) struct RemoveQuery {
    #[serde(default)]
    pub force: Option<bool>,
}

#[derive(Deserialize)]
pub(super) struct FileQuery {
    pub path: String,
    /// Archive shape hint, as the client sent it.
    ///
    /// Deliberately a `String`, not a `bool`: serde would reject the whole
    /// request on a malformed value, while the runner's `parseSourceIsDir`
    /// (apps/runner/pkg/api/controllers/boxlite_files.go) reads anything
    /// unparseable as "no hint". Both servers answer the same clients, so both
    /// have to be that forgiving.
    #[serde(default)]
    pub source_is_dir: Option<String>,
}
