//! Request/response serde structs matching the OpenAPI schema.
//!
//! These are wire-format types for the REST API. They are converted
//! to/from core types (BoxInfo, BoxOptions, etc.) at the boundary.

use std::collections::HashMap;

use boxlite_shared::errors::BoxliteError;
use serde::{Deserialize, Serialize};

use crate::litebox::BoxStatus;
use crate::litebox::snapshot_mgr::SnapshotInfo;
use crate::runtime::advanced_options::ContainerCapabilities;
use crate::runtime::options::{CloneOptions, ExportOptions, SnapshotOptions};

// ============================================================================
// Error Model
// ============================================================================

#[derive(Debug, Deserialize)]
pub(crate) struct ErrorResponse {
    pub error: ErrorModel,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FlatErrorResponse {
    pub message: String,
    pub code: Option<String>,
}

impl FlatErrorResponse {
    /// A body with no `code` decodes to the empty string, not to a
    /// synthesized one. Inventing `"internal"` here made every codeless
    /// refusal — which is every cloud exception that does not set the field —
    /// arrive as a server fault, and hid the HTTP status from the mapper for
    /// good: the status-driven fallback was unreachable behind it.
    pub(crate) fn into_error_model(self) -> ErrorModel {
        ErrorModel {
            message: self.message,
            error_type: "HttpError".to_string(),
            code: self.code.unwrap_or_default(),
            request_id: None,
        }
    }
}

/// Wire shape received from the server.
///
/// - `message` — human-readable error text.
/// - `error_type` — stable PascalCase identifier (K8s `Status.reason`
///   style). Mirrors `BoxliteError::http().1` server-side.
/// - `code` — stable snake_case machine identifier (Stripe `code` style).
///   Refines the status-derived baseline in [`super::error`]; an empty
///   string means the server named no code and that baseline stands.
///   `error_type` is kept for diagnostics / logging.
/// - `request_id` — propagated from server's `X-Request-Id` middleware
///   when present; absent on older servers (forward-compat).
#[derive(Debug, Deserialize)]
pub(crate) struct ErrorModel {
    pub message: String,
    /// Preserved for diagnostics / log enrichment; the variant is chosen
    /// from the status and `code`, never from this field.
    #[serde(rename = "type", default)]
    #[allow(dead_code)]
    pub error_type: String,
    /// Absent on any server that does not name a code. Defaulting to the
    /// empty string keeps such a body a decodable envelope, so the mapper
    /// classifies it by status and reports the server's own sentence; as a
    /// required field it made the whole body unparseable and the caller was
    /// handed the raw JSON instead.
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub request_id: Option<String>,
}

// ============================================================================
// Configuration
// ============================================================================

/// Server configuration & capabilities — the `GET /v1/config` response.
/// Matches the `ServerConfig` schema in `openapi/box.openapi.yaml`.
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct ServerConfig {
    pub capabilities: Option<ServerCapabilities>,
}

#[allow(dead_code)] // Constructed via serde::Deserialize
#[derive(Debug, Deserialize, Clone, Default)]
pub(crate) struct ServerCapabilities {
    pub linux_capabilities_enabled: Option<bool>,
    pub snapshots_enabled: Option<bool>,
    pub clone_enabled: Option<bool>,
    pub export_enabled: Option<bool>,
    pub import_enabled: Option<bool>,
}

// ============================================================================
// Box
// ============================================================================

#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rootfs_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_size_gb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<CreateBoxNetworkSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<CreateBoxSecret>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<Vec<CreateBoxVolumeSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detach: Option<bool>,
    /// A terminal for the main command (`run -t`). Only sent when asked for:
    /// the server rejects unknown fields, so an older one would 400 on it —
    /// which is the right failure. Degrading `-it` to pipes silently, as the
    /// alternative, is how this whole class of bug happens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced: Option<CreateBoxAdvancedOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_stop: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_delete: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_resume: Option<bool>,
}

impl CreateBoxRequest {
    pub fn from_options(
        options: &crate::runtime::options::BoxOptions,
        name: Option<String>,
    ) -> Self {
        use crate::runtime::options::RootfsSpec;

        let (image, rootfs_path) = match &options.rootfs {
            RootfsSpec::Image(img) => (Some(img.clone()), None),
            RootfsSpec::RootfsPath(path) => (None, Some(path.clone())),
        };

        let env = if options.env.is_empty() {
            None
        } else {
            Some(options.env.iter().cloned().collect())
        };

        let secrets = if options.secrets.is_empty() {
            None
        } else {
            Some(options.secrets.iter().map(CreateBoxSecret::from).collect())
        };
        let volumes = if options.volumes.is_empty() {
            None
        } else {
            Some(
                options
                    .volumes
                    .iter()
                    .map(CreateBoxVolumeSpec::from)
                    .collect(),
            )
        };

        // SecurityOptions is intentionally NOT carried on the wire.
        // Sandbox security is the operator's policy and is set
        // server-side; the REST surface deliberately exposes no knob
        // for clients to relax it (would be a sandbox-escape vector).
        // Local-mode callers and CLI/Go/C SDKs continue to honour
        // `BoxOptions.advanced.security` because those run under the
        // caller's own trust boundary.

        Self {
            name,
            image,
            rootfs_path,
            cpus: options.cpus,
            memory_mib: options.memory_mib,
            disk_size_gb: options.disk_size_gb,
            working_dir: options.working_dir.clone(),
            env,
            // net_bandwidth is intentionally NOT carried on the wire. Shaping is
            // done by the local gvproxy bridge; a remote server enforces its own
            // network policy, so there is no field for a client to set. The
            // matching refusal lives in BoxOptions::sanitize_remote.
            network: Some(CreateBoxNetworkSpec::from_options(
                &options.network,
                &options.inbound_network,
            )),
            entrypoint: options.entrypoint.clone(),
            cmd: options.cmd.clone(),
            user: options.user.clone(),
            secrets,
            volumes,
            detach: Some(options.detach),
            tty: options.tty.then_some(true),
            // `Some`, not "non-empty", decides whether this reaches the wire:
            // an explicitly empty policy is still explicit, and collapsing it
            // into the same shape as "never touched" would leave the server
            // unable to tell the two apart.
            advanced: options.advanced.capabilities().map(|capabilities| {
                CreateBoxAdvancedOptions {
                    capabilities: capabilities.clone(),
                }
            }),
            // The deprecated remove-on-stop flag was never applied by the cloud
            // control-plane mapper. Keep remote defaults unchanged and only send
            // the modern lifecycle fields when callers explicitly configure them.
            auto_stop: options.auto_stop,
            auto_delete: options.auto_delete,
            auto_resume: options.auto_resume,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxAdvancedOptions {
    pub capabilities: ContainerCapabilities,
}

/// A mount on the wire. Only managed volumes exist here — a REST server has no
/// host filesystem to bind from, so `BoxOptions::sanitize_remote` refuses a host
/// bind at create and this type has no field for one.
#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxVolumeSpec {
    pub managed_volume: String,
    pub guest_path: String,
    pub read_only: bool,
}

impl From<&crate::runtime::options::VolumeSpec> for CreateBoxVolumeSpec {
    fn from(volume: &crate::runtime::options::VolumeSpec) -> Self {
        Self {
            // `BoxOptions::sanitize_remote` runs first and refuses any mount whose
            // `managed_volume` is unset, so the default is unreachable rather
            // than a fallback: an empty string would be a selector the server
            // can never resolve. `From` cannot report that, which is why the
            // check lives at create instead of here.
            managed_volume: volume.managed_volume.clone().unwrap_or_default(),
            guest_path: volume.guest_path.clone(),
            read_only: volume.read_only,
        }
    }
}

/// Wire shape sent to the server when creating a box.
///
/// `Legacy` is the pre-split flat shape `{"mode","allow_net"}`, accepted by
/// all server versions. `Nested` carries explicit inbound/outbound directions
/// and requires a server with #1199+ support.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(crate) enum CreateBoxNetworkSpec {
    /// Pre-split shape — sent when inbound is at its default so servers that
    /// predate the inbound/outbound split (#1199) keep working.
    Legacy(CreateBoxLegacyNetworkSpec),
    /// Inbound/outbound shape — sent when inbound is explicitly configured.
    Nested(CreateBoxNestedNetworkSpec),
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxLegacyNetworkSpec {
    pub mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_net: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxNestedNetworkSpec {
    pub outbound: CreateBoxOutboundNetworkSpec,
    pub inbound: CreateBoxInboundNetworkSpec,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxOutboundNetworkSpec {
    pub mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_net: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxInboundNetworkSpec {
    pub mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_net: Vec<String>,
}

fn mode_str(mode: crate::runtime::options::NetworkMode) -> String {
    match mode {
        crate::runtime::options::NetworkMode::Enabled => "enabled".to_string(),
        crate::runtime::options::NetworkMode::Disabled => "disabled".to_string(),
    }
}

impl CreateBoxNetworkSpec {
    fn from_options(
        outbound: &crate::runtime::options::NetworkSpec,
        inbound: &crate::runtime::options::NetworkSpec,
    ) -> Self {
        let config = crate::runtime::options::NetworkConfig::from_specs(outbound, inbound);
        // Use the legacy flat shape when inbound is at its default (Enabled,
        // empty allowlist). Any explicit inbound configuration requires the
        // nested shape and a server that understands it (#1199+).
        match inbound {
            crate::runtime::options::NetworkSpec::Enabled { allow_net } if allow_net.is_empty() => {
                Self::Legacy(CreateBoxLegacyNetworkSpec {
                    mode: mode_str(config.outbound.mode),
                    allow_net: config.outbound.allow_net,
                })
            }
            _ => Self::Nested(CreateBoxNestedNetworkSpec {
                outbound: CreateBoxOutboundNetworkSpec {
                    mode: mode_str(config.outbound.mode),
                    allow_net: config.outbound.allow_net,
                },
                inbound: CreateBoxInboundNetworkSpec {
                    mode: mode_str(config.inbound.mode),
                    allow_net: config.inbound.allow_net,
                },
            }),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct CreateBoxSecret {
    pub name: String,
    pub value: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    pub placeholder: String,
}

impl From<&crate::runtime::options::Secret> for CreateBoxSecret {
    fn from(secret: &crate::runtime::options::Secret) -> Self {
        Self {
            name: secret.name.clone(),
            value: secret.value.clone(),
            hosts: secret.hosts.clone(),
            placeholder: secret.placeholder.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct BoxResponse {
    pub box_id: String,
    pub name: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
    /// Absent when the server recorded no activity for the box, and from any
    /// server too old to publish it.
    #[serde(default)]
    pub last_activity_at: Option<String>,
    pub pid: Option<u32>,
    pub image: String,
    pub cpus: u8,
    pub memory_mib: u32,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Absent while the box's main command is still running. An older server
    /// omits it entirely, which reads the same as "still running" — the
    /// honest answer when it cannot say.
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default = "default_auto_stop")]
    pub auto_stop: u32,
    #[serde(default = "default_auto_delete")]
    pub auto_delete: u32,
    #[serde(default = "default_auto_resume")]
    pub auto_resume: bool,
}

impl BoxResponse {
    pub fn to_box_info(&self) -> boxlite_shared::errors::BoxliteResult<crate::BoxInfo> {
        use crate::runtime::id::BoxID;

        let id = BoxID::parse(&self.box_id).ok_or_else(|| {
            BoxliteError::Internal(format!(
                "REST server returned unparseable box_id: {:?} (must be non-empty, ≤{} chars, URL/path-safe)",
                self.box_id,
                BoxID::MAX_LENGTH,
            ))
        })?;

        let status = parse_box_status(&self.status);

        let created_at = chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());

        let last_updated = chrono::DateTime::parse_from_rfc3339(&self.updated_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());

        // Unlike the two timestamps above, an unparseable value becomes `None`
        // rather than `now()`: activity dated to this instant would read as a
        // box that was just used, which is exactly the wrong answer for the
        // idleness this field describes.
        let last_activity_at = self
            .last_activity_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        Ok(crate::BoxInfo {
            id,
            name: self.name.clone(),
            status,
            created_at,
            last_updated,
            pid: self.pid,
            image: self.image.clone(),
            cpus: self.cpus,
            memory_mib: self.memory_mib,
            // The remote REST surface intentionally does not publish local
            // host bindings. Missing network metadata is therefore distinct
            // from a locally verified, resolved-empty publication list.
            network: None,
            labels: self.labels.clone(),
            auto_stop: self.auto_stop,
            auto_delete: self.auto_delete,
            auto_resume: self.auto_resume,
            health_status: crate::litebox::HealthStatus::new(), // REST API doesn't provide health status
            exit_code: self.exit_code,
            // Like `network`, the remote REST surface does not publish this
            // local start timestamp; `None` means "not known here", not
            // "the box never entered Running".
            started_at: None,
            last_activity_at,
        })
    }
}

fn default_auto_stop() -> u32 {
    900
}

fn default_auto_delete() -> u32 {
    0
}

fn default_auto_resume() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListBoxesResponse {
    pub boxes: Vec<BoxResponse>,
    #[allow(dead_code)]
    pub next_page_token: Option<String>,
}

// ============================================================================
// Named volumes (`/v1/volumes`)
// ============================================================================

/// Body for `POST /v1/volumes`.
///
/// `name` is omitted from the wire when unset, so an unnamed create still sends
/// `{}` and the server falls back to the assigned id.
#[derive(Debug, Serialize)]
pub(crate) struct CreateVolumeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// A single volume as returned by the REST API.
#[derive(Debug, Deserialize)]
pub(crate) struct VolumeResponse {
    pub id: String,
    /// Required by the spec, but defaulted so a pre-name server still parses.
    #[serde(default)]
    pub name: String,
    pub created_at: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

impl VolumeResponse {
    pub fn to_volume_info(&self) -> crate::volumes::VolumeInfo {
        let created_at = chrono::DateTime::parse_from_rfc3339(&self.created_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());

        crate::volumes::VolumeInfo {
            id: self.id.clone(),
            // A server that predates the name field leaves it blank; fall back
            // to the id, which is what an unnamed volume is called anyway.
            name: if self.name.is_empty() {
                self.id.clone()
            } else {
                self.name.clone()
            },
            created_at,
            size_bytes: self.size_bytes,
        }
    }
}

/// Response for `GET /v1/volumes`.
#[derive(Debug, Deserialize)]
pub(crate) struct ListVolumesResponse {
    pub volumes: Vec<VolumeResponse>,
}

// ============================================================================
// Snapshot / Clone / Export
// ============================================================================

#[derive(Debug, Serialize)]
pub(crate) struct CreateSnapshotRequest {
    pub name: String,
}

impl CreateSnapshotRequest {
    pub fn from_options(_options: &SnapshotOptions, name: &str) -> Self {
        Self {
            name: name.to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub(crate) struct SnapshotResponse {
    pub id: String,
    pub box_id: String,
    pub name: String,
    pub created_at: i64,
    pub container_disk_bytes: u64,
    pub size_bytes: u64,
}

impl SnapshotResponse {
    pub fn to_snapshot_info(&self) -> SnapshotInfo {
        SnapshotInfo {
            id: self.id.clone(),
            box_id: self.box_id.clone(),
            name: self.name.clone(),
            created_at: self.created_at,
            disk_info: crate::disk::DiskInfo {
                base_path: String::new(),
                container_disk_bytes: self.container_disk_bytes,
                size_bytes: self.size_bytes,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListSnapshotsResponse {
    pub snapshots: Vec<SnapshotResponse>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CloneBoxRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl CloneBoxRequest {
    pub fn from_options(_options: &CloneOptions, name: Option<&str>) -> Self {
        Self {
            name: name.map(|s| s.to_string()),
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct ExportBoxRequest {}

impl ExportBoxRequest {
    pub fn from_options(_options: &ExportOptions) -> Self {
        Self {}
    }
}

// ============================================================================
// Execution
// ============================================================================

#[derive(Debug, Serialize)]
pub(crate) struct ExecRequest {
    pub command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub tty: bool,
}

impl ExecRequest {
    pub fn from_command(cmd: &crate::BoxCommand) -> Self {
        let env = cmd
            .env
            .as_ref()
            .map(|pairs| pairs.iter().cloned().collect::<HashMap<String, String>>());
        let timeout_seconds = cmd.timeout.map(|d| d.as_secs_f64());

        Self {
            command: cmd.command.clone(),
            args: cmd.args.clone(),
            env,
            timeout_seconds,
            working_dir: cmd.working_dir.clone(),
            tty: cmd.tty,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExecResponse {
    pub execution_id: String,
}

/// Response from `GET /executions/{exec_id}` (status fallback).
///
/// Mirrors the OpenAPI `ExecutionInfo` schema; used by the WS attach
/// path when the connection terminates without an `exit` frame so
/// callers still observe the real exit code.
#[derive(Debug, Deserialize)]
pub(crate) struct ExecutionStatusResponse {
    pub status: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Serialize)]
pub(crate) struct SignalRequestBody {
    pub signal: i32,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResizeRequestBody {
    pub cols: u32,
    pub rows: u32,
}

// ============================================================================
// Metrics
// ============================================================================

#[derive(Debug, Deserialize)]
pub(crate) struct RuntimeMetricsResponse {
    #[serde(default)]
    pub boxes_created_total: u64,
    #[serde(default)]
    pub boxes_failed_total: u64,
    #[serde(default)]
    pub boxes_stopped_total: u64,
    #[serde(default)]
    #[allow(dead_code)]
    pub num_running_boxes: u64,
    #[serde(default)]
    pub total_commands_executed: u64,
    #[serde(default)]
    pub total_exec_errors: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BoxMetricsResponse {
    #[serde(default)]
    pub commands_executed_total: u64,
    #[serde(default)]
    pub exec_errors_total: u64,
    #[serde(default)]
    pub bytes_sent_total: u64,
    #[serde(default)]
    pub bytes_received_total: u64,
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub network_bytes_sent: Option<u64>,
    pub network_bytes_received: Option<u64>,
    pub network_tcp_connections: Option<u64>,
    pub network_tcp_errors: Option<u64>,
    pub boot_timing: Option<BootTimingResponse>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct BootTimingResponse {
    pub total_create_ms: Option<u64>,
    pub guest_boot_ms: Option<u64>,
    pub filesystem_setup_ms: Option<u64>,
    pub image_prepare_ms: Option<u64>,
    pub guest_rootfs_ms: Option<u64>,
    pub box_config_ms: Option<u64>,
    pub box_spawn_ms: Option<u64>,
    pub container_init_ms: Option<u64>,
}

fn parse_box_status(status: &str) -> BoxStatus {
    match status {
        "configured" => BoxStatus::Configured,
        "running" => BoxStatus::Running,
        "stopping" => BoxStatus::Stopping,
        "stopped" => BoxStatus::Stopped,
        "paused" => BoxStatus::Paused,
        "failed" => BoxStatus::Failed,
        _ => BoxStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::options::BoxOptions;

    #[test]
    fn test_create_box_request_serialization() {
        use crate::runtime::options::{BoxOptions, NetworkSpec, RootfsSpec};

        // inbound at default → legacy flat shape, accepted by all server versions.
        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("python:3.11".into()),
            cpus: Some(2),
            memory_mib: Some(512),
            network: NetworkSpec::Enabled {
                allow_net: vec!["api.openai.com".into()],
            },
            // inbound_network left at default: Enabled { allow_net: [] }
            ..Default::default()
        };
        let mut req = CreateBoxRequest::from_options(&opts, Some("mybox".into()));
        req.secrets = Some(vec![CreateBoxSecret {
            name: "openai".into(),
            value: "sk-test".into(),
            hosts: vec!["api.openai.com".into()],
            placeholder: "<BOXLITE_SECRET:openai>".into(),
        }]);
        req.auto_stop = Some(900);
        req.auto_delete = Some(604800);

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"name\":\"mybox\""));
        assert!(json.contains("\"image\":\"python:3.11\""));
        assert!(json.contains("\"cpus\":2"));
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Legacy flat shape: top-level mode/allow_net, no outbound/inbound nesting.
        assert_eq!(value["network"]["mode"], "enabled");
        assert_eq!(value["network"]["allow_net"][0], "api.openai.com");
        assert!(value["network"]["outbound"].is_null());
        assert!(json.contains("\"secrets\""));
        // None fields should be skipped
        assert!(!json.contains("rootfs_path"));
        assert!(!json.contains("disk_size_gb"));
    }

    #[test]
    fn test_create_box_request_from_options() {
        use crate::runtime::options::{BoxOptions, NetworkSpec, RootfsSpec, Secret, VolumeSpec};

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            cpus: Some(4),
            memory_mib: Some(1024),
            network: NetworkSpec::Enabled {
                allow_net: vec!["api.openai.com".into()],
            },
            inbound_network: NetworkSpec::Disabled,
            secrets: vec![Secret {
                name: "openai".into(),
                value: "sk-test".into(),
                hosts: vec!["api.openai.com".into()],
                placeholder: "<BOXLITE_SECRET:openai>".into(),
            }],
            volumes: vec![VolumeSpec::managed_volume("volume-123", "/data")],
            auto_stop: Some(1800),
            auto_delete: Some(604800),
            ..Default::default()
        };
        let req = CreateBoxRequest::from_options(&opts, Some("test-box".into()));
        assert_eq!(req.name.as_deref(), Some("test-box"));
        assert_eq!(req.image.as_deref(), Some("alpine:latest"));
        assert!(req.rootfs_path.is_none());
        assert_eq!(req.cpus, Some(4));
        assert_eq!(req.memory_mib, Some(1024));
        // inbound is Disabled (non-default) → nested shape required.
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["network"]["outbound"]["mode"], "enabled");
        assert_eq!(
            json["network"]["outbound"]["allow_net"][0],
            "api.openai.com"
        );
        assert_eq!(json["network"]["inbound"]["mode"], "disabled");
        assert!(json["network"]["mode"].is_null());
        assert_eq!(req.secrets.as_ref().map(Vec::len), Some(1));
        let volume = &req.volumes.as_ref().unwrap()[0];
        assert_eq!(volume.managed_volume, "volume-123");
        assert_eq!(volume.guest_path, "/data");
        assert!(!volume.read_only);
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(
            json["volumes"],
            serde_json::json!([{
                "managed_volume": "volume-123",
                "guest_path": "/data",
                "read_only": false
            }])
        );
        assert_eq!(req.auto_stop, Some(1800));
        assert_eq!(req.auto_delete, Some(604800));
        assert_eq!(
            req.secrets.as_ref().unwrap()[0].placeholder,
            "<BOXLITE_SECRET:openai>"
        );
    }

    #[test]
    fn test_create_box_request_carries_container_capabilities() {
        let mut advanced = crate::AdvancedBoxOptions::default();
        advanced
            .set_capabilities(Some(ContainerCapabilities {
                add: vec!["SYS_ADMIN".into()],
                drop: vec!["CAP_NET_RAW".into()],
            }))
            .unwrap();
        let opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        let req = CreateBoxRequest::from_options(&opts, None);
        let advanced = req.advanced.as_ref().expect("custom policy is serialized");
        assert_eq!(advanced.capabilities.add, ["SYS_ADMIN"]);
        assert_eq!(advanced.capabilities.drop, ["CAP_NET_RAW"]);

        let json = serde_json::to_value(&req).expect("serialize create request");
        assert_eq!(
            json["advanced"]["capabilities"],
            serde_json::json!({"add": ["SYS_ADMIN"], "drop": ["CAP_NET_RAW"]})
        );

        let defaults = CreateBoxRequest::from_options(&BoxOptions::default(), None);
        let defaults_json = serde_json::to_value(defaults).expect("serialize defaults");
        assert!(defaults_json.get("advanced").is_none());
    }

    /// An explicit, empty policy is still explicit — it must reach the wire,
    /// not collapse into the same "advanced omitted" shape an untouched
    /// field produces, or the server can no longer tell the two apart.
    #[test]
    fn test_create_box_request_carries_an_explicitly_empty_capability_policy() {
        let mut advanced = crate::AdvancedBoxOptions::default();
        advanced
            .set_capabilities(Some(ContainerCapabilities::default()))
            .unwrap();
        let opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        let req = CreateBoxRequest::from_options(&opts, None);
        assert!(
            req.advanced.is_some(),
            "an explicit empty policy must still be serialized"
        );

        let json = serde_json::to_value(&req).expect("serialize create request");
        assert!(json.get("advanced").is_some());
    }

    /// An unnamed create must not put `"name": null` on the wire — the spec
    /// marks the body optional and its schema `additionalProperties: false`.
    #[test]
    fn unnamed_volume_create_omits_the_name_key() {
        let json = serde_json::to_value(CreateVolumeRequest { name: None }).unwrap();
        assert_eq!(json, serde_json::json!({}));

        let named = serde_json::to_value(CreateVolumeRequest {
            name: Some("my-data".into()),
        })
        .unwrap();
        assert_eq!(named, serde_json::json!({"name": "my-data"}));
    }

    /// A server that predates the name field sends no `name`. Falling back to
    /// the id keeps `VolumeInfo.name` mountable rather than empty — an unnamed
    /// volume is called by its id server-side anyway.
    #[test]
    fn volume_response_without_a_name_falls_back_to_the_id() {
        let response: VolumeResponse =
            serde_json::from_str(r#"{"id":"vol_01K2EXAMPLE","created_at":"2026-08-26T00:00:00Z"}"#)
                .expect("a pre-name server response must still parse");
        assert_eq!(response.to_volume_info().name, "vol_01K2EXAMPLE");

        let named: VolumeResponse = serde_json::from_str(
            r#"{"id":"vol_01K2EXAMPLE","name":"my-data","created_at":"2026-08-26T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(named.to_volume_info().name, "my-data");
    }

    /// The reference reaches the wire byte-for-byte as the caller wrote it,
    /// whether it is a server-assigned id or a name. The server resolves both,
    /// so the client neither narrows nor decorates it — there is no scheme to
    /// add, and nothing here may rewrite the value.
    #[test]
    fn managed_volume_reaches_wire_verbatim_by_id_or_by_name() {
        use crate::runtime::options::{BoxOptions, VolumeSpec};

        for reference in ["vol_01K2EXAMPLE", "my-data"] {
            let opts = BoxOptions {
                volumes: vec![VolumeSpec::managed_volume(reference, "/data")],
                ..Default::default()
            };

            let req = CreateBoxRequest::from_options(&opts, None);
            let volume = &req.volumes.as_ref().unwrap()[0];
            assert_eq!(volume.managed_volume, reference);
            assert_eq!(volume.guest_path, "/data");

            let json = serde_json::to_value(&req).unwrap();
            assert_eq!(json["volumes"][0]["managed_volume"], reference);
            assert!(
                json["volumes"][0].get("source").is_none(),
                "the wire has no `source` field any more: {}",
                json["volumes"][0]
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_auto_remove_does_not_change_rest_lifecycle_defaults() {
        for auto_remove in [false, true] {
            let opts = BoxOptions {
                auto_remove,
                auto_delete: None,
                ..Default::default()
            };
            let req = CreateBoxRequest::from_options(&opts, None);
            assert_eq!(req.auto_stop, None);
            assert_eq!(req.auto_delete, None);
        }

        let modern = BoxOptions {
            auto_remove: true,
            auto_stop: Some(900),
            auto_delete: Some(3600),
            ..Default::default()
        };
        let req = CreateBoxRequest::from_options(&modern, None);
        assert_eq!(req.auto_stop, Some(900));
        assert_eq!(req.auto_delete, Some(3600));
    }

    #[test]
    fn test_create_box_request_from_options_disabled_network() {
        use crate::runtime::options::{BoxOptions, NetworkSpec, RootfsSpec};

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            network: NetworkSpec::Disabled,
            ..Default::default()
        };

        let req = CreateBoxRequest::from_options(&opts, None);
        // inbound at default → legacy flat shape.
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["network"]["mode"], "disabled");
        // NetworkSpec::Disabled only overrides outbound; inbound keeps its
        // default (Enabled/public) — not present in the legacy flat shape.
        assert!(json["network"]["outbound"].is_null());
    }

    /// REST is intentionally a "the server picks the security policy"
    /// surface: even a caller with a non-default `SecurityOptions`
    /// (here: `maximum`) MUST serialize a wire body that carries
    /// neither `security` nor `security_settings`. Re-introducing
    /// either field would let a client toggle the operator's
    /// sandbox configuration — a sandbox-escape vector. The test
    /// asserts both the absence of the JSON keys and the absence
    /// of the preset name to catch either field being smuggled
    /// back in under a renamed alias.
    #[test]
    fn test_create_box_request_never_carries_security_on_the_wire() {
        use crate::SecurityOptions;
        use crate::runtime::advanced_options::AdvancedBoxOptions;
        use crate::runtime::options::{BoxOptions, RootfsSpec};

        let mut advanced = AdvancedBoxOptions::default();
        advanced.security = SecurityOptions::enabled();
        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            advanced,
            ..Default::default()
        };
        let req = CreateBoxRequest::from_options(&opts, None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("security"),
            "wire form must NOT carry any security knob; got: {json}"
        );
    }

    /// Bandwidth shaping is done by the local gvproxy bridge, so the wire form
    /// has no field for it and a local value must not leak into the request.
    #[test]
    fn test_create_box_request_never_carries_net_bandwidth() {
        use crate::runtime::options::{BoxOptions, NetBandwidth, RootfsSpec};

        let opts = BoxOptions {
            rootfs: RootfsSpec::Image("alpine:latest".into()),
            net_bandwidth: NetBandwidth {
                tx_kbps: Some(10_000),
                rx_kbps: Some(20_000),
            },
            ..Default::default()
        };
        let req = CreateBoxRequest::from_options(&opts, None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(
            !json.contains("bandwidth") && !json.contains("kbps"),
            "wire form must NOT carry a bandwidth cap; got: {json}"
        );
    }

    #[test]
    fn test_box_response_deserialization() {
        let json = r#"{
            "box_id": "01J0000000000000000000000A",
            "name": "mybox",
            "status": "running",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:01:00Z",
            "pid": 1234,
            "image": "python:3.11",
            "cpus": 2,
            "memory_mib": 512,
            "labels": {}
        }"#;
        let resp: BoxResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.box_id, "01J0000000000000000000000A");
        assert_eq!(resp.name.as_deref(), Some("mybox"));
        assert_eq!(resp.status, "running");
        assert_eq!(resp.pid, Some(1234));
        assert_eq!(resp.cpus, 2);
        assert_eq!(resp.auto_stop, 900);
        assert_eq!(resp.auto_delete, 0);
    }

    #[test]
    fn test_box_response_to_box_info() {
        let resp = BoxResponse {
            box_id: "01J0000000000000000000000A".to_string(),
            name: Some("mybox".to_string()),
            status: "running".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:01:00Z".to_string(),
            last_activity_at: None,
            pid: Some(1234),
            image: "python:3.11".to_string(),
            cpus: 2,
            memory_mib: 512,
            labels: HashMap::new(),
            exit_code: None,
            auto_stop: 1800,
            auto_delete: 604800,
            auto_resume: true,
        };
        let info = resp.to_box_info().expect("valid ULID box_id should parse");
        assert_eq!(info.name.as_deref(), Some("mybox"));
        assert_eq!(info.image, "python:3.11");
        assert_eq!(info.cpus, 2);
        assert_eq!(info.memory_mib, 512);
        assert!(info.network.is_none());
        assert_eq!(info.auto_stop, 1800);
        assert_eq!(info.auto_delete, 604800);
    }

    #[test]
    fn box_response_to_box_info_reports_last_activity() {
        let mut resp = BoxResponse {
            box_id: "01J0000000000000000000000A".to_string(),
            name: None,
            status: "running".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:01:00Z".to_string(),
            last_activity_at: Some("2024-01-01T00:02:30Z".to_string()),
            pid: None,
            image: "python:3.11".to_string(),
            cpus: 2,
            memory_mib: 512,
            labels: HashMap::new(),
            exit_code: None,
            auto_stop: 1800,
            auto_delete: 0,
            auto_resume: true,
        };

        let info = resp.to_box_info().expect("valid box_id should parse");
        assert_eq!(
            info.last_activity_at.map(|at| at.to_rfc3339()),
            Some("2024-01-01T00:02:30+00:00".to_string())
        );

        // An absent value means "no activity recorded", and an unparseable one
        // must not be rounded up to "active now".
        resp.last_activity_at = None;
        assert!(resp.to_box_info().unwrap().last_activity_at.is_none());
        resp.last_activity_at = Some("yesterday".to_string());
        assert!(resp.to_box_info().unwrap().last_activity_at.is_none());
    }

    #[test]
    fn test_box_response_to_box_info_uuid() {
        // Some servers may return UUIDs as box_id (not just 12-char Base62 / 26-char ULID).
        // Verify the SDK accepts them and round-trips the id verbatim.
        let resp = BoxResponse {
            box_id: "d406c59d-eb09-4bc3-9b3a-62455c7e8f32".to_string(),
            name: Some("uuid-box".to_string()),
            status: "running".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:01:00Z".to_string(),
            last_activity_at: None,
            pid: Some(5678),
            image: "alpine:latest".to_string(),
            cpus: 1,
            memory_mib: 256,
            labels: HashMap::new(),
            exit_code: None,
            auto_stop: 900,
            auto_delete: 0,
            auto_resume: true,
        };
        let info = resp.to_box_info().expect("UUID box_id should parse");
        assert_eq!(info.id.as_str(), "d406c59d-eb09-4bc3-9b3a-62455c7e8f32");
        assert_eq!(info.name.as_deref(), Some("uuid-box"));
    }

    #[test]
    fn test_box_response_to_box_info_unparseable() {
        // BoxID is opaque server-issued — most strings parse. The only
        // shapes that must propagate a parse error are the ones that
        // would corrupt URLs / on-disk paths if accepted: empty, oversized,
        // or containing path-traversal / URL-unsafe characters. This is
        // the belt-and-suspenders against the old silent-mint bug.
        let mk = |bad_id: &str| BoxResponse {
            box_id: bad_id.to_string(),
            name: None,
            status: "running".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:01:00Z".to_string(),
            last_activity_at: None,
            pid: None,
            image: "alpine:latest".to_string(),
            cpus: 1,
            memory_mib: 256,
            labels: HashMap::new(),
            exit_code: None,
            auto_stop: 900,
            auto_delete: 0,
            auto_resume: true,
        };
        assert!(mk("").to_box_info().is_err(), "empty");
        assert!(mk("a/b").to_box_info().is_err(), "slash");
        assert!(mk("a b").to_box_info().is_err(), "whitespace");
        assert!(mk("a.b").to_box_info().is_err(), "dot");
    }

    #[test]
    fn test_exec_request_serialization() {
        let req = ExecRequest {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), "print('hi')".to_string()],
            env: None,
            timeout_seconds: Some(30.0),
            working_dir: Some("/app".to_string()),
            tty: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"command\":\"python3\""));
        assert!(json.contains("\"timeout_seconds\":30.0"));
        assert!(json.contains("\"working_dir\":\"/app\""));
    }

    #[test]
    fn test_error_response_deserialization() {
        let json = r#"{
            "error": {
                "message": "box not found",
                "type": "NotFoundError",
                "code": "not_found",
                "request_id": "req_01HZK"
            }
        }"#;
        let resp: ErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.error.message, "box not found");
        assert_eq!(resp.error.error_type, "NotFoundError");
        assert_eq!(resp.error.code, "not_found");
        assert_eq!(resp.error.request_id.as_deref(), Some("req_01HZK"));
    }

    #[test]
    fn test_runtime_metrics_deserialization() {
        let json = r#"{
            "boxes_created_total": 10,
            "boxes_failed_total": 1,
            "boxes_stopped_total": 5,
            "num_running_boxes": 4,
            "total_commands_executed": 100,
            "total_exec_errors": 2
        }"#;
        let resp: RuntimeMetricsResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.boxes_created_total, 10);
        assert_eq!(resp.total_commands_executed, 100);
    }

    #[test]
    fn test_box_status_transient_mapping() {
        let mut resp = BoxResponse {
            box_id: "01J0000000000000000000000A".to_string(),
            name: Some("mybox".to_string()),
            status: "snapshotting".to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:01:00Z".to_string(),
            last_activity_at: None,
            pid: Some(1234),
            image: "python:3.11".to_string(),
            cpus: 2,
            memory_mib: 512,
            labels: HashMap::new(),
            exit_code: None,
            auto_stop: 900,
            auto_delete: 0,
            auto_resume: true,
        };

        // Legacy transient statuses map to Unknown (no longer valid)
        assert_eq!(resp.to_box_info().unwrap().status, BoxStatus::Unknown);
        resp.status = "paused".to_string();
        assert_eq!(resp.to_box_info().unwrap().status, BoxStatus::Paused);
    }

    #[test]
    fn test_server_config_capabilities_deserialization() {
        let json = r#"{
            "capabilities": {
                "snapshots_enabled": true,
                "linux_capabilities_enabled": true,
                "clone_enabled": false,
                "export_enabled": true
            }
        }"#;
        let resp: ServerConfig = serde_json::from_str(json).unwrap();
        let caps = resp.capabilities.unwrap();
        assert_eq!(caps.linux_capabilities_enabled, Some(true));
        assert_eq!(caps.snapshots_enabled, Some(true));
        assert_eq!(caps.clone_enabled, Some(false));
        assert_eq!(caps.export_enabled, Some(true));
    }

    #[test]
    fn test_snapshot_response_to_snapshot_info() {
        let resp = SnapshotResponse {
            id: "01JABCDEF0123456789XYZABCD".to_string(),
            box_id: "01J0000000000000000000000A".to_string(),
            name: "snap1".to_string(),
            created_at: 1_700_000_000,
            container_disk_bytes: 2048,
            size_bytes: 4096,
        };

        let info = resp.to_snapshot_info();
        assert_eq!(info.name, "snap1");
        assert_eq!(info.disk_info.base_path, "");
        assert_eq!(info.disk_info.size_bytes, 4096);
    }
}
