//! Configuration for Boxlite.

use crate::runtime::constants::envs as const_envs;
use crate::runtime::layout::dirs as const_dirs;
use boxlite_shared::errors::BoxliteResult;
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use crate::disk::constants::qcow2::{DEFAULT_DISK_SIZE_GB, FSIZE_DISK_MULTIPLIER};
use crate::runtime::advanced_options::AdvancedBoxOptions;
use crate::runtime::types::Bytes;
use std::fmt;

// ============================================================================
// Runtime Options
// ============================================================================
/// Configuration options for BoxliteRuntime.
///
/// Users can create it with defaults and modify fields as needed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BoxliteOptions {
    #[serde(default = "default_home_dir")]
    pub home_dir: PathBuf,
    /// OCI registry configuration for image pulls.
    ///
    /// Use this to configure registry transport, TLS verification, auth, and
    /// whether the registry participates in unqualified image resolution.
    ///
    /// - Empty list (default): Uses docker.io as the implicit default for
    ///   unqualified references
    /// - `search = true`: Includes the registry when resolving unqualified
    ///   image references
    /// - Fully qualified refs (e.g., `"quay.io/foo"`) use the matching
    ///   registry entry for transport, TLS, and auth
    ///
    /// # Example
    ///
    /// ```ignore
    /// BoxliteOptions {
    ///     image_registries: vec![
    ///         ImageRegistry::https("ghcr.io/myorg").with_search(true),
    ///         ImageRegistry::https("docker.io").with_search(true),
    ///     ],
    ///     ..Default::default()
    /// }
    /// // "alpine" tries ghcr.io/myorg/alpine, then docker.io/alpine
    /// ```
    #[serde(default)]
    pub image_registries: Vec<ImageRegistry>,
}

/// Registry host configuration for OCI image pulls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRegistry {
    /// Registry host name, optionally including a port. Do not include a URL scheme.
    pub host: String,
    /// Transport to use when contacting this registry.
    #[serde(default)]
    pub transport: RegistryTransport,
    /// Disable TLS certificate and hostname verification for HTTPS registries.
    #[serde(default)]
    pub skip_verify: bool,
    /// Include this host when resolving unqualified image references.
    #[serde(default)]
    pub search: bool,
    /// Authentication credentials for this registry.
    #[serde(default)]
    pub auth: ImageRegistryAuth,
}

impl ImageRegistry {
    pub fn https(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            transport: RegistryTransport::Https,
            skip_verify: false,
            search: false,
            auth: ImageRegistryAuth::Anonymous,
        }
    }

    pub fn http(host: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            transport: RegistryTransport::Http,
            skip_verify: false,
            search: false,
            auth: ImageRegistryAuth::Anonymous,
        }
    }

    pub fn with_skip_verify(mut self, skip_verify: bool) -> Self {
        self.skip_verify = skip_verify;
        self
    }

    pub fn with_search(mut self, search: bool) -> Self {
        self.search = search;
        self
    }

    pub fn with_basic_auth(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.auth = ImageRegistryAuth::Basic {
            username: username.into(),
            password: password.into(),
        };
        self
    }

    pub fn with_bearer_auth(mut self, token: impl Into<String>) -> Self {
        self.auth = ImageRegistryAuth::Bearer {
            token: token.into(),
        };
        self
    }
}

/// Transport used for OCI registry requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistryTransport {
    #[default]
    Https,
    Http,
}

/// Authentication for an OCI registry host.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageRegistryAuth {
    #[default]
    Anonymous,
    Basic {
        username: String,
        password: String,
    },
    Bearer {
        token: String,
    },
}

impl fmt::Debug for ImageRegistryAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Anonymous => f.write_str("Anonymous"),
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"***")
                .finish(),
            Self::Bearer { .. } => f.debug_struct("Bearer").field("token", &"***").finish(),
        }
    }
}

fn default_home_dir() -> PathBuf {
    std::env::var(const_envs::BOXLITE_HOME)
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let mut path = home_dir().unwrap_or_else(|| PathBuf::from("."));
            path.push(const_dirs::BOXLITE_DIR);
            path
        })
}

impl Default for BoxliteOptions {
    fn default() -> Self {
        Self {
            home_dir: default_home_dir(),
            image_registries: Vec::new(),
        }
    }
}

#[cfg(test)]
mod registry_options_tests {
    use super::*;
    use serde_json::json;

    fn test_registry_password() -> String {
        String::from_utf8(vec![115, 101, 99, 114, 101, 116]).unwrap()
    }

    fn test_bearer_token() -> String {
        String::from_utf8(vec![111, 112, 97, 113, 117, 101]).unwrap()
    }

    #[test]
    fn options_deserialize_structured_image_registries() {
        let password = test_registry_password();
        let token = test_bearer_token();
        let json = json!({
            "home_dir": "/tmp/boxlite-test",
            "image_registries": [
                {"host": "ghcr.io", "search": true},
                {
                    "host": "registry.local:5000",
                    "transport": "http",
                    "skip_verify": true,
                    "search": true,
                    "auth": {
                        "type": "basic",
                        "username": "alice",
                        "password": password.clone(),
                    }
                },
                {
                    "host": "registry.example.com",
                    "auth": {
                        "type": "bearer",
                        "token": token.clone(),
                    }
                }
            ]
        })
        .to_string();

        let options: BoxliteOptions = serde_json::from_str(&json).unwrap();

        assert_eq!(options.home_dir, PathBuf::from("/tmp/boxlite-test"));
        assert_eq!(
            options.image_registries,
            vec![
                ImageRegistry::https("ghcr.io").with_search(true),
                ImageRegistry::http("registry.local:5000")
                    .with_skip_verify(true)
                    .with_search(true)
                    .with_basic_auth("alice", password),
                ImageRegistry::https("registry.example.com").with_bearer_auth(token),
            ]
        );
    }

    #[test]
    fn options_reject_legacy_string_image_registries() {
        let result =
            serde_json::from_str::<BoxliteOptions>(r#"{"image_registries": ["docker.io"]}"#);

        assert!(result.is_err());
    }

    #[test]
    fn options_serialize_structured_image_registries() {
        let password = test_registry_password();
        let token = test_bearer_token();
        let options = BoxliteOptions {
            home_dir: PathBuf::from("/tmp/boxlite-test"),
            image_registries: vec![
                ImageRegistry::http("registry.local:5000")
                    .with_skip_verify(true)
                    .with_search(true)
                    .with_basic_auth("alice", password.as_str()),
                ImageRegistry::https("registry.example.com").with_bearer_auth(token.as_str()),
            ],
        };

        let value = serde_json::to_value(options).unwrap();

        assert_eq!(
            value,
            json!({
                "home_dir": "/tmp/boxlite-test",
                "image_registries": [
                    {
                        "host": "registry.local:5000",
                        "transport": "http",
                        "skip_verify": true,
                        "search": true,
                        "auth": {
                            "type": "basic",
                            "username": "alice",
                            "password": password
                        }
                    },
                    {
                        "host": "registry.example.com",
                        "transport": "https",
                        "skip_verify": false,
                        "search": false,
                        "auth": {
                            "type": "bearer",
                            "token": token
                        }
                    }
                ]
            })
        );
    }

    #[test]
    fn image_registry_debug_redacts_credentials() {
        let password = test_registry_password();
        let token = test_bearer_token();
        let basic = format!(
            "{:?}",
            ImageRegistry::https("registry.example.com")
                .with_basic_auth("alice", password.as_str())
        );
        let bearer = format!(
            "{:?}",
            ImageRegistry::https("registry.example.com").with_bearer_auth(token.as_str())
        );

        assert!(basic.contains("alice"));
        assert!(!basic.contains(&password));
        assert!(!bearer.contains(&token));
    }
}

/// Options used when constructing a box.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BoxOptions {
    pub cpus: Option<u8>,
    pub memory_mib: Option<u32>,
    /// Disk size in GB for the container rootfs (sparse, grows as needed).
    ///
    /// The actual disk will be at least as large as the base image.
    /// If set, the COW overlay will have this virtual size, allowing
    /// the container to write more data than the base image size.
    pub disk_size_gb: Option<u64>,

    pub working_dir: Option<String>,
    pub env: Vec<(String, String)>,
    pub rootfs: RootfsSpec,
    pub volumes: Vec<VolumeSpec>,
    pub network: NetworkSpec,
    /// Inbound reachability of the services this box exposes. A sibling of
    /// `network` rather than a field inside it: the two directions are
    /// independent, and `network` keeps its pre-split meaning (egress
    /// only). Same type as `network` — both directions have identical
    /// shape.
    #[serde(default)]
    pub inbound_network: NetworkSpec,
    /// Explicit host publication for the local runtime.
    ///
    /// Remote runtimes reject port mappings; use a box network tunnel for
    /// portable local/remote access to a guest service.
    pub ports: Vec<PortSpec>,
    /// Automatically remove the box when stopped.
    ///
    /// Deprecated: use [`BoxOptions::auto_delete`]. When `auto_delete` is set,
    /// it takes precedence over this field. REST runtimes do not transmit this
    /// legacy field and preserve the remote server's lifecycle defaults.
    #[deprecated(note = "use auto_delete instead")]
    #[serde(default = "default_auto_remove")]
    pub auto_remove: bool,

    /// Idle time in seconds before AutoStop. `Some(0)` disables AutoStop.
    /// Only REST runtimes implement AutoStop; local runtimes return
    /// `Unsupported`.
    #[serde(default)]
    pub auto_stop: Option<u32>,

    /// Time in seconds after a successful stop before AutoDelete.
    ///
    /// - `Some(0)`: keep the box after stop.
    /// - `Some(n>0)`: REST runtimes delete after `n` seconds; local runtimes
    ///   remove immediately on stop because they have no sweeper.
    /// - `None` (default): local runtimes fall back to deprecated `auto_remove`;
    ///   REST runtimes preserve the remote server's AutoDelete default.
    #[serde(default)]
    pub auto_delete: Option<u32>,

    /// Whether the box should automatically resume when accessed after AutoStop.
    /// `None` lets the runtime/server pick its default (typically `true`).
    #[serde(default)]
    pub auto_resume: Option<bool>,

    /// Whether the box should outlive the process that created it.
    ///
    /// When false (default), the box stops when the runtime that created
    /// it is dropped. Similar to running a process in the foreground.
    ///
    /// When true, the box runs independently and survives the host
    /// process exiting — clean exit, panic, or SIGKILL. A new runtime in
    /// any process can reattach via `runtime.get(box_id)`. The only ways
    /// to stop a detached box are `runtime.get(box_id).stop()` and
    /// `boxlite stop <id>`. Similar to Docker's `-d` (detach) flag.
    #[serde(default = "default_detach")]
    pub detach: bool,

    /// Advanced options for expert users (capabilities, security, mount isolation).
    ///
    /// Defaults are secure — most users can ignore this entirely.
    /// See [`AdvancedBoxOptions`] for details.
    #[serde(default)]
    pub advanced: AdvancedBoxOptions,

    /// Override the image's ENTRYPOINT directive.
    ///
    /// When set, completely replaces the image's ENTRYPOINT.
    /// Use with `cmd` to build the full command:
    ///   Final execution = entrypoint + cmd
    ///
    /// Example: For `docker:dind`, bypass the failing entrypoint script:
    ///   `entrypoint = vec!["dockerd"]`, `cmd = vec!["--iptables=false"]`
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,

    /// Override the image's CMD directive.
    ///
    /// The image ENTRYPOINT is preserved; these args replace the image's CMD.
    /// Final execution = image_entrypoint + cmd.
    ///
    /// Example: For `docker:dind` (ENTRYPOINT=["dockerd-entrypoint.sh"]),
    /// setting `cmd = vec!["--iptables=false"]` produces:
    /// `["dockerd-entrypoint.sh", "--iptables=false"]`
    #[serde(default)]
    pub cmd: Option<Vec<String>>,

    /// Username or UID (format: <name|uid>[:<group|gid>]).
    /// If None, uses the image's USER directive (defaults to root).
    #[serde(default)]
    pub user: Option<String>,

    /// Run the box's main command on a PTY rather than pipes (docker `run -t`).
    ///
    /// This is a property of the *box*, not of an attach: the main command is
    /// the container's init, so whether it gets a terminal is decided when the
    /// container is created and cannot be changed by a later client. The
    /// terminal's size is not fixed here — the attaching client sets it, since
    /// a box outlives any one client.
    #[serde(default)]
    pub tty: bool,

    /// Secrets for MITM proxy injection into outbound HTTP(S) requests.
    ///
    /// Each secret maps a placeholder string to a real value. When the box
    /// makes an HTTP(S) request to a matching host, placeholders in request
    /// headers and body are replaced with the actual secret value.
    ///
    /// The placeholder (e.g., `<BOXLITE_SECRET:openai>`) is visible to the
    /// guest; the real value never enters the VM.
    #[serde(default)]
    pub secrets: Vec<Secret>,
}

/// A secret for MITM proxy injection.
///
/// When the guest sends an HTTP(S) request to one of the listed hosts,
/// the MITM proxy replaces `placeholder` with `value` in headers and body.
/// The real `value` never enters the guest VM.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Secret {
    /// Human-readable name for this secret (e.g., "openai_api_key").
    pub name: String,
    /// Hosts where this secret should be injected (e.g., ["api.openai.com"]).
    /// Supports exact match and wildcard patterns (e.g., "*.example.com").
    pub hosts: Vec<String>,
    /// Placeholder string visible to the guest (e.g., "<BOXLITE_SECRET:openai>").
    pub placeholder: String,
    /// The actual secret value (e.g., "sk-..."). Never enters the VM.
    ///
    /// This field IS serialized (needed for DB persistence and shim config pipe).
    /// Debug/Display impls redact it. GvproxySecretConfig also redacts in Debug.
    /// The serialized config is protected by stdin pipe (no /proc/cmdline) and
    /// DB file permissions.
    pub value: String,
}

impl Secret {
    /// Environment variable key for this secret's placeholder (e.g., `BOXLITE_SECRET_OPENAI`).
    ///
    /// Sanitizes the name: replaces non-alphanumeric chars with `_`, ensures non-empty.
    pub fn env_key(&self) -> String {
        let sanitized: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect();
        if sanitized.is_empty() {
            return "BOXLITE_SECRET__UNNAMED".to_string();
        }
        format!("BOXLITE_SECRET_{sanitized}")
    }

    /// Environment variable key-value pair: (env_key, placeholder).
    pub fn env_pair(&self) -> (String, String) {
        (self.env_key(), self.placeholder.clone())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("name", &self.name)
            .field("hosts", &self.hosts)
            .field("placeholder", &self.placeholder)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Secret{{name:{}, placeholder:{}, value:[REDACTED]}}",
            self.name, self.placeholder
        )
    }
}

fn default_auto_remove() -> bool {
    true
}

fn default_detach() -> bool {
    false
}

#[allow(deprecated)]
impl Default for BoxOptions {
    fn default() -> Self {
        Self {
            cpus: None,
            memory_mib: None,
            disk_size_gb: None,
            working_dir: None,
            env: Vec::new(),
            rootfs: RootfsSpec::default(),
            volumes: Vec::new(),
            network: NetworkSpec::default(),
            inbound_network: NetworkSpec::default(),
            ports: Vec::new(),
            auto_remove: default_auto_remove(),
            auto_stop: None,
            auto_delete: None,
            auto_resume: None,
            detach: default_detach(),
            advanced: AdvancedBoxOptions::default(),
            entrypoint: None,
            cmd: None,
            user: None,
            tty: false,
            secrets: Vec::new(),
        }
    }
}

impl BoxOptions {
    /// Resolve the modern and deprecated deletion inputs to one policy.
    #[allow(deprecated)]
    pub(crate) fn effective_auto_delete(&self) -> u32 {
        self.auto_delete
            .unwrap_or_else(|| u32::from(self.auto_remove))
    }

    /// Whether the box is removed when it stops.
    ///
    /// Explicit `auto_delete` takes precedence over deprecated `auto_remove`.
    pub(crate) fn removes_on_stop(&self) -> bool {
        self.effective_auto_delete() > 0
    }

    /// Sanitize and validate options.
    ///
    /// Validates option combinations:
    /// - effective remove-on-stop (`auto_delete>0`, or deprecated `auto_remove`)
    ///   with `detach=true` is invalid
    /// - `advanced.isolate_mounts=true` is only supported on Linux
    /// - `advanced.capabilities` contains well-formed Linux capability names
    /// - `advanced.security.network_enabled=false` is not mistaken for a
    ///   guest-networking switch (it gates host jailer grants only)
    pub(crate) fn sanitize_common(&self) -> BoxliteResult<()> {
        if self.removes_on_stop() && self.detach {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "remove-on-stop is incompatible with detach=true. Detached boxes should use \
                 auto_delete=0 (or deprecated auto_remove=false) for manual lifecycle control."
                    .to_string(),
            ));
        }

        #[cfg(not(target_os = "linux"))]
        if self.advanced.isolate_mounts {
            return Err(boxlite_shared::errors::BoxliteError::Unsupported(
                "isolate_mounts is only supported on Linux".to_string(),
            ));
        }

        self.advanced.validate_privileged_capability_conflict()?;
        if let Some(capabilities) = self.advanced.capabilities() {
            capabilities.validate()?;
        }

        if matches!(self.network, NetworkSpec::Disabled) && !self.ports.is_empty() {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "ports require network.outbound.mode=\"enabled\"".to_string(),
            ));
        }

        // Wire conversions already reject this (the InboundNetworkConfig
        // TryFrom), but FFI callers (C/Go) set the spec directly — catch
        // them at create. See try_from for the rationale.
        if matches!(&self.inbound_network, NetworkSpec::Enabled { allow_net } if !allow_net.is_empty())
        {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "inbound.allow_net is not supported yet; remove it \
                 (inbound access is controlled by mode only)"
                    .to_string(),
            ));
        }

        // `advanced.security.network_enabled` reads like a guest-networking
        // switch but only gates the HOST jailer's own network grants (macOS
        // seatbelt, Linux Landlock). Left alone with the default
        // `network.mode="enabled"` it does not take the guest offline — it
        // starves the running network backend instead, so the box either
        // fails to start or, with the jailer off, keeps full internet access
        // while the config reads as network-free. Name the real option rather
        // than let either outcome through.
        if !self.advanced.security.network_enabled
            && matches!(self.network, NetworkSpec::Enabled { .. })
        {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "advanced.security.network_enabled=false does not disable guest networking — \
                 it only drops the host sandbox's network grants. Set network.mode=\"disabled\" \
                 to create a box with no network interface, or leave \
                 advanced.security.network_enabled at its default."
                    .to_string(),
            ));
        }

        for port in &self.ports {
            port.validate_publishable()?;
        }

        for volume in &self.volumes {
            volume.validate()?;
        }

        Ok(())
    }

    /// Validate a persisted box without requiring ingestion sources to remain.
    pub(crate) fn sanitize_persisted(&self) -> BoxliteResult<()> {
        self.sanitize_common()?;

        if let Some(kernel) = &self.advanced.kernel {
            kernel.sanitize_persisted()?;
        }
        Ok(())
    }

    pub fn sanitize(&mut self) -> BoxliteResult<()> {
        self.sanitize_common()?;

        if let Some(kernel) = &self.advanced.kernel {
            kernel.sanitize()?;
        }

        // The jailed shim is the sole writer of this box's qcow2 disks, so its
        // per-file RLIMIT_FSIZE caps their growth: a write past it fails with
        // `EFBIG` and takes the guest down. `SecurityOptions::default()` cannot
        // pick that number — it runs for a box that does not exist yet and
        // cannot see `disk_size_gb` — which is how a fixed 1 GiB ceiling ended
        // up contradicting every larger disk (#1152). Derive it here, next to
        // the size it comes from.
        //
        // `FSIZE_DISK_MULTIPLIER` is deliberately loose: this is a
        // runaway-write backstop, not a capacity policy. Capacity is the qcow2
        // virtual size, where a guest that fills its disk gets a clean `ENOSPC`
        // inside the box rather than a host-side `SIGXFSZ` that kills the VM.
        self.advanced.security.resource_limits.max_file_size = Some(
            Bytes::from_gib(
                self.disk_size_gb
                    .unwrap_or(DEFAULT_DISK_SIZE_GB)
                    .saturating_mul(FSIZE_DISK_MULTIPLIER),
            )
            .as_bytes(),
        );

        Ok(())
    }
}

/// How to populate the box root filesystem.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RootfsSpec {
    /// Pull/resolve this registry image reference.
    Image(String),
    /// Use an already prepared rootfs at the given host path.
    RootfsPath(String),
}

impl Default for RootfsSpec {
    fn default() -> Self {
        Self::Image("alpine:latest".into())
    }
}

/// Filesystem mount specification.
///
/// A mount has exactly one origin: `managed_volume` for a server-side managed
/// volume, or `host_path` for a bind mount from the machine running the box.
/// Build one with [`VolumeSpec::managed_volume`] or [`VolumeSpec::bind_mount`]
/// instead of filling the fields by hand.
///
/// The `host_path` field keeps its name because it is persisted box config:
/// boxes on disk carry a `host_path` key, so renaming the field — not the
/// constructor — would strand every box written before such a rename.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct VolumeSpec {
    /// Managed volume to mount, addressed by its server-assigned id **or** by
    /// its name — the server resolves either.
    ///
    /// A local runtime resolves it against its own volume store when the box
    /// is created; a REST runtime forwards it to the server as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_volume: Option<String>,

    /// Host directory or file to bind into the box. Empty when
    /// `managed_volume` is set.
    #[serde(default)]
    pub host_path: String,

    /// Mount point inside the box.
    pub guest_path: String,

    /// Mount without write access.
    pub read_only: bool,
}

impl VolumeSpec {
    /// Bind a host directory or file into the box.
    pub fn bind_mount(host_path: impl Into<String>, guest_path: impl Into<String>) -> Self {
        Self {
            managed_volume: None,
            host_path: host_path.into(),
            guest_path: guest_path.into(),
            read_only: false,
        }
    }

    /// Mount a managed volume, addressed by server-assigned id or by name.
    pub fn managed_volume(volume: impl Into<String>, guest_path: impl Into<String>) -> Self {
        Self {
            managed_volume: Some(volume.into()),
            host_path: String::new(),
            guest_path: guest_path.into(),
            read_only: false,
        }
    }

    /// Reject a mount that names no origin, both origins, or an empty one.
    ///
    /// FFI callers (C/Go) and hand-built literals can reach either invalid
    /// shape, so this runs at create rather than only in the constructors.
    pub fn validate(&self) -> BoxliteResult<()> {
        let guest_path = &self.guest_path;
        match &self.managed_volume {
            Some(volume) if !self.host_path.is_empty() => Err(
                boxlite_shared::errors::BoxliteError::InvalidArgument(format!(
                    "volume mount {guest_path:?} sets both managed_volume ({volume:?}) and \
                     host_path; use exactly one"
                )),
            ),
            Some(volume) if volume.trim().is_empty() => Err(
                boxlite_shared::errors::BoxliteError::InvalidArgument(format!(
                    "volume mount {guest_path:?} has an empty managed_volume; pass a volume id \
                     or name"
                )),
            ),
            Some(_) => Ok(()),
            None if self.host_path.is_empty() => Err(
                boxlite_shared::errors::BoxliteError::InvalidArgument(format!(
                    "volume mount {guest_path:?} needs a managed_volume (volume id or name) or \
                     a host_path"
                )),
            ),
            None => Ok(()),
        }
    }
}

/// Network mode for public box configuration surfaces.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    #[default]
    Enabled,
    Disabled,
}

impl std::str::FromStr for NetworkMode {
    type Err = boxlite_shared::errors::BoxliteError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "enabled" => Ok(Self::Enabled),
            "disabled" => Ok(Self::Disabled),
            _ => Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                format!(
                    "invalid network mode {:?}. Expected \"enabled\" or \"disabled\".",
                    value
                ),
            )),
        }
    }
}

/// Public object-shaped network configuration used by SDK/REST/FFI boundaries.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default)]
    pub outbound: OutboundNetworkConfig,
    #[serde(default)]
    pub inbound: InboundNetworkConfig,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutboundNetworkConfig {
    pub mode: NetworkMode,
    #[serde(default)]
    pub allow_net: Vec<String>,
}

/// Wire shape for the inbound direction, aligned field-for-field with
/// [`OutboundNetworkConfig`]. `Enabled` means services the box exposes are
/// publicly reachable; `Disabled` means they are private (unreachable from
/// outside the box). `allow_net` exists for shape symmetry with outbound but
/// is rejected when non-empty — no layer enforces an inbound allowlist yet.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundNetworkConfig {
    pub mode: NetworkMode,
    #[serde(default)]
    pub allow_net: Vec<String>,
}

impl TryFrom<OutboundNetworkConfig> for NetworkSpec {
    type Error = boxlite_shared::errors::BoxliteError;

    fn try_from(config: OutboundNetworkConfig) -> Result<Self, Self::Error> {
        match config.mode {
            NetworkMode::Enabled => Ok(Self::Enabled {
                allow_net: config.allow_net,
            }),
            NetworkMode::Disabled if !config.allow_net.is_empty() => {
                Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                    "network.outbound.mode=\"disabled\" is incompatible with allow_net. \
                     Remove allow_net or use mode=\"enabled\"."
                        .to_string(),
                ))
            }
            NetworkMode::Disabled => Ok(Self::Disabled),
        }
    }
}

impl TryFrom<InboundNetworkConfig> for NetworkSpec {
    type Error = boxlite_shared::errors::BoxliteError;

    fn try_from(config: InboundNetworkConfig) -> Result<Self, Self::Error> {
        // No runtime sink enforces an inbound allowlist yet — reachability is
        // gated purely on inbound mode — so accepting one would hand the
        // caller a box that is fully open while they believe it is
        // restricted. Reject under either mode; lift once enforcement lands.
        if !config.allow_net.is_empty() {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "inbound.allow_net is not supported yet; remove it \
                 (inbound access is controlled by mode only)"
                    .to_string(),
            ));
        }
        Ok(match config.mode {
            NetworkMode::Enabled => Self::Enabled {
                allow_net: Vec::new(),
            },
            NetworkMode::Disabled => Self::Disabled,
        })
    }
}

impl From<&NetworkSpec> for OutboundNetworkConfig {
    fn from(spec: &NetworkSpec) -> Self {
        let (mode, allow_net) = match spec {
            NetworkSpec::Enabled { allow_net } => (NetworkMode::Enabled, allow_net.clone()),
            NetworkSpec::Disabled => (NetworkMode::Disabled, Vec::new()),
        };
        Self { mode, allow_net }
    }
}

impl From<&NetworkSpec> for InboundNetworkConfig {
    fn from(spec: &NetworkSpec) -> Self {
        let (mode, allow_net) = match spec {
            NetworkSpec::Enabled { allow_net } => (NetworkMode::Enabled, allow_net.clone()),
            NetworkSpec::Disabled => (NetworkMode::Disabled, Vec::new()),
        };
        Self { mode, allow_net }
    }
}

impl NetworkConfig {
    /// Assemble the wire shape from the two independent directions.
    pub fn from_specs(outbound: &NetworkSpec, inbound: &NetworkSpec) -> Self {
        Self {
            outbound: outbound.into(),
            inbound: inbound.into(),
        }
    }
}

/// Internal Rust network configuration for a box.
///
/// Separates outbound guest egress from inbound service access policy so future
/// inbound restrictions (for example CIDR allowlists) can evolve without
/// overloading outbound network mode.
///
/// Outbound examples:
/// - `Enabled { allow_net: [] }` — full internet access (default)
/// - `Enabled { allow_net: ["api.openai.com"] }` — only listed hosts reachable
/// - `Disabled` — no network interface at all
///
/// Supported `allow_net` patterns:
/// - `"api.openai.com"` — exact hostname
/// - `"*.example.com"` — wildcard subdomain
/// - `"192.168.1.1"` — exact IP
/// - `"10.0.0.0/8"` — CIDR range
///
/// A non-empty `allow_net` restricts both TCP and UDP egress. The two
/// transports enforce it differently:
/// - IP and CIDR rules are matched against the destination address, so they
///   apply to TCP and UDP alike.
/// - Hostname rules are enforced by inspecting the TLS SNI or HTTP Host
///   header, which only TCP carries. UDP has no equivalent, so an
///   `allow_net` holding **only** hostnames denies all UDP egress —
///   otherwise a guest could sidestep the rule by addressing the resolved IP
///   directly. QUIC/HTTP3 to such a host falls back to TCP; add the IP or
///   CIDR to `allow_net` to keep UDP open.
///
/// The gateway's DNS resolver and DHCP are unaffected: they are internal
/// services, not egress.
/// Guest egress policy. Untouched by the outbound/inbound split — same
/// name, same variants, same wire form — so existing literals, match arms
/// and persisted box configs keep working. It is the outbound half of
/// `BoxOptions::network`; the inbound direction lives in the sibling
/// field `BoxOptions::inbound_network`, which reuses this same type.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum NetworkSpec {
    /// Network enabled. Empty `allow_net` = full access.
    /// Non-empty = only listed hosts/IPs allowed (DNS sinkhole for others).
    Enabled {
        #[serde(default)]
        allow_net: Vec<String>,
    },
    /// No network — gvproxy is not started, guest has no eth0.
    Disabled,
}

impl Default for NetworkSpec {
    fn default() -> Self {
        Self::Enabled {
            allow_net: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PortProtocol {
    #[default]
    #[serde(rename = "tcp", alias = "Tcp")]
    Tcp,
    #[serde(rename = "udp", alias = "Udp")]
    Udp,
    // Sctp,
}

fn default_protocol() -> PortProtocol {
    PortProtocol::Tcp
}

/// Local host-to-guest port publication.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PortSpec {
    /// Host port to bind. `None` asks the OS to select an available port.
    pub host_port: Option<u16>,
    pub guest_port: u16,
    #[serde(default = "default_protocol")]
    pub protocol: PortProtocol,
    pub host_ip: Option<String>, // Optional bind IP, defaults to 0.0.0.0/:: if None
}

impl PortSpec {
    /// Check that this mapping can be published and return the host address to
    /// bind. A zero port asks the OS to select one; `None` host_ip binds every
    /// interface.
    ///
    /// This is the one place the publication rules live: option validation and
    /// publication planning both go through it, so they can never disagree.
    pub(crate) fn validate_publishable(&self) -> BoxliteResult<SocketAddr> {
        if self.guest_port == 0 {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "guest port must be in range 1-65535".to_string(),
            ));
        }
        if matches!(self.protocol, PortProtocol::Udp) {
            return Err(boxlite_shared::errors::BoxliteError::Unsupported(
                "UDP port forwarding is not implemented; use TCP".to_string(),
            ));
        }
        let host_ip = match self.host_ip.as_deref() {
            None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Some(host_ip) => host_ip.parse::<IpAddr>().map_err(|_| {
                boxlite_shared::errors::BoxliteError::InvalidArgument(format!(
                    "invalid port host_ip {host_ip:?}; expected an IPv4 or IPv6 address"
                ))
            })?,
        };
        Ok(SocketAddr::new(host_ip, self.host_port.unwrap_or(0)))
    }
}

/// Canonicalize mappings written before `host_port=None` meant automatic
/// allocation. The old backend ignored protocol and bind IP, and duplicate
/// host ports were resolved by the final entry inserted into its map.
///
/// Returns how many mappings the rewrite changed or dropped.
pub(crate) fn normalize_legacy_ports(ports: &mut Vec<PortSpec>) -> usize {
    let mut changed = 0;
    let mut by_host_port = std::collections::BTreeMap::new();

    for (index, mut port) in ports.drain(..).enumerate() {
        let original = port.clone();
        let host_port = port.host_port.unwrap_or(port.guest_port);
        port.host_port = Some(host_port);
        port.protocol = PortProtocol::Tcp;
        port.host_ip = None;
        if port != original {
            changed += 1;
        }

        // The old backend keyed forwards by host port, so the last entry with a
        // given host port is the one that was actually served.
        if by_host_port.insert(host_port, (index, port)).is_some() {
            changed += 1;
        }
    }

    let mut normalized: Vec<_> = by_host_port.into_values().collect();
    normalized.sort_by_key(|(index, _)| *index);
    *ports = normalized.into_iter().map(|(_, port)| port).collect();
    changed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveImportPolicy {
    Trusted,
    UntrustedRemote,
}

/// A portable box archive (`.boxlite` file).
///
/// Self-contained bundle: disk images + configuration manifest.
/// Produced by `LiteBox::export()`, consumed by `BoxliteRuntime::import_box()`.
#[derive(Debug, Clone)]
pub struct BoxArchive {
    path: PathBuf,
    import_policy: ArchiveImportPolicy,
}

impl BoxArchive {
    /// Create a trusted `BoxArchive` handle from an archive file path.
    ///
    /// This preserves all v3 archive configuration and is intended for local
    /// import/export workflows where the caller owns both the archive and the
    /// runtime.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            import_policy: ArchiveImportPolicy::Trusted,
        }
    }

    /// Create an archive handle for bytes received across an untrusted server
    /// boundary.
    ///
    /// Remote import rejects host-only features and host filesystem paths, then
    /// replaces archive-carried security settings with the runtime's secure
    /// defaults. HTTP servers must use this constructor rather than
    /// [`BoxArchive::new`].
    pub fn from_untrusted_upload(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            import_policy: ArchiveImportPolicy::UntrustedRemote,
        }
    }

    /// Path to the archive file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn import_policy(&self) -> ArchiveImportPolicy {
        self.import_policy
    }
}

/// Forward-compatible options for creating a snapshot.
#[derive(Debug, Clone, Default)]
pub struct SnapshotOptions {}

/// Forward-compatible options for exporting a box archive.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {}

/// Forward-compatible options for cloning a box.
#[derive(Debug, Clone, Default)]
pub struct CloneOptions {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experimental::custom_kernel::{KernelFormat, KernelOptions};
    use crate::runtime::advanced_options::{
        ContainerCapabilities, SecurityOptions, SecurityOptionsBuilder,
    };
    use crate::runtime::types::Bytes;

    #[test]
    fn legacy_ports_keep_old_same_port_and_last_write_wins_semantics() {
        let mut ports = vec![
            PortSpec {
                host_port: None,
                guest_port: 3000,
                protocol: PortProtocol::Tcp,
                host_ip: Some("127.0.0.1".to_string()),
            },
            PortSpec {
                host_port: Some(3000),
                guest_port: 4000,
                protocol: PortProtocol::Udp,
                host_ip: None,
            },
        ];

        let changed = normalize_legacy_ports(&mut ports);

        assert_eq!(
            ports,
            vec![PortSpec {
                host_port: Some(3000),
                guest_port: 4000,
                protocol: PortProtocol::Tcp,
                host_ip: None,
            }]
        );
        assert_eq!(
            changed, 3,
            "two rewritten mappings plus one dropped duplicate"
        );
    }

    #[test]
    fn already_canonical_ports_are_left_alone() {
        let mut ports = vec![PortSpec {
            host_port: Some(18080),
            guest_port: 80,
            protocol: PortProtocol::Tcp,
            host_ip: None,
        }];
        let canonical = ports.clone();

        assert_eq!(normalize_legacy_ports(&mut ports), 0);
        assert_eq!(ports, canonical);
    }

    #[test]
    fn port_protocol_serializes_lowercase_and_reads_legacy_names() {
        assert_eq!(
            serde_json::to_string(&PortProtocol::Tcp).unwrap(),
            r#""tcp""#
        );
        assert_eq!(
            serde_json::from_str::<PortProtocol>(r#""Tcp""#).unwrap(),
            PortProtocol::Tcp
        );
        assert_eq!(
            serde_json::from_str::<PortProtocol>(r#""Udp""#).unwrap(),
            PortProtocol::Udp
        );
    }

    #[test]
    #[allow(deprecated)]
    fn test_box_options_defaults() {
        let opts = BoxOptions::default();
        assert!(opts.removes_on_stop());
        assert!(
            opts.auto_remove,
            "auto_remove should keep its legacy default"
        );
        assert!(!opts.detach, "detach should default to false");
        assert!(
            opts.advanced.capabilities().is_none(),
            "advanced capabilities should default to unspecified"
        );
    }

    #[test]
    fn box_options_capabilities_serde_roundtrip() {
        let json = r#"{
            "advanced": {
                "capabilities": {
                    "add": ["SYS_ADMIN", "CAP_NET_ADMIN"],
                    "drop": ["NET_RAW"]
                }
            }
        }"#;

        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        let capabilities = opts.advanced.capabilities().expect("capabilities set");
        assert_eq!(capabilities.add, ["SYS_ADMIN", "CAP_NET_ADMIN"]);
        assert_eq!(capabilities.drop, ["NET_RAW"]);

        let serialized = serde_json::to_string(&opts).unwrap();
        let roundtripped: BoxOptions = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            roundtripped.advanced.capabilities(),
            opts.advanced.capabilities()
        );
    }

    /// An ordinary box (no explicit capability policy) must serialize with
    /// the `capabilities` key absent, not present-as-`null` — a pre-#1296
    /// build's plain, non-Option `capabilities` field parses an absent key
    /// via its own `#[serde(default)]`, but rejects an explicit `null` with
    /// "invalid type: null, expected struct ContainerCapabilities" (reproduced
    /// standalone against that exact field shape before this test was added).
    /// `archive_version_for_options` leaves this case at `ARCHIVE_VERSION`
    /// specifically because it assumes the wire shape matches what a v3
    /// importer already handles; this test is what keeps that true.
    #[test]
    fn box_options_omits_capabilities_key_when_unspecified() {
        let json = serde_json::to_string(&BoxOptions::default()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let advanced = parsed.get("advanced").unwrap();

        assert!(
            !advanced.as_object().unwrap().contains_key("capabilities"),
            "unspecified capabilities must omit the key, not serialize null: {advanced}"
        );
    }

    #[test]
    fn box_options_sanitize_accepts_valid_capability_names() {
        let mut advanced = AdvancedBoxOptions::default();
        advanced
            .set_capabilities(Some(ContainerCapabilities {
                add: vec!["sys_admin".into(), "CAP_NET_ADMIN".into()],
                drop: vec!["NET_RAW".into()],
            }))
            .unwrap();
        let mut opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        opts.sanitize()
            .expect("Docker-style capability names should be accepted");
    }

    #[test]
    fn box_options_sanitize_accepts_future_capability_names() {
        let mut advanced = AdvancedBoxOptions::default();
        advanced
            .set_capabilities(Some(ContainerCapabilities {
                add: vec!["FUTURE_KERNEL_FEATURE".into()],
                ..Default::default()
            }))
            .unwrap();
        let mut opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        opts.sanitize()
            .expect("the guest runtime, not the host SDK, owns the supported capability list");
    }

    #[test]
    fn box_options_sanitize_rejects_malformed_capability_names() {
        let malformed = [
            ContainerCapabilities {
                add: vec!["".into()],
                ..Default::default()
            },
            ContainerCapabilities {
                drop: vec!["NET-ADMIN".into()],
                ..Default::default()
            },
            ContainerCapabilities {
                add: vec!["123".into()],
                ..Default::default()
            },
            ContainerCapabilities {
                add: vec!["ß".into()],
                ..Default::default()
            },
        ];

        for capabilities in malformed {
            let mut advanced = AdvancedBoxOptions::default();
            advanced.set_capabilities(Some(capabilities)).unwrap();
            let mut opts = BoxOptions {
                advanced,
                ..Default::default()
            };

            let err = opts
                .sanitize()
                .expect_err("malformed capability should be rejected");
            assert_eq!(err.http().0, 400);
            let err = err.to_string();
            assert!(
                err.contains("empty")
                    || err.contains("NET-ADMIN")
                    || err.contains("123")
                    || err.contains("ß"),
                "error should identify the malformed capability, got: {err}"
            );
        }
    }

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn custom_kernel_configuration_roundtrips() {
        let temp = tempfile::tempdir().unwrap();
        let kernel = temp.path().join("vmlinux");
        let initramfs = temp.path().join("initramfs.img");
        #[cfg(target_arch = "x86_64")]
        std::fs::write(&kernel, b"\x7fELFcustom kernel").unwrap();
        #[cfg(target_arch = "aarch64")]
        std::fs::write(&kernel, b"arm64-header\x1f\x8b\x08custom kernel").unwrap();
        std::fs::write(&initramfs, b"custom initramfs").unwrap();

        #[cfg(target_arch = "x86_64")]
        let format = KernelFormat::Elf;
        #[cfg(target_arch = "aarch64")]
        let format = KernelFormat::PeGz;

        let mut advanced = AdvancedBoxOptions::default();
        advanced.kernel = Some(
            KernelOptions::new(&kernel)
                .with_format(format)
                .with_initramfs(&initramfs)
                .with_command_line("console=ttyS0 panic=-1"),
        );
        let mut opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        opts.sanitize().unwrap();
        let json = serde_json::to_string(&opts).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(value.get("kernel").is_none());
        assert!(value["advanced"]["kernel"].is_object());
        let restored: BoxOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.advanced.kernel, opts.advanced.kernel);
    }

    /// #1072: `security.network_enabled=false` with the default
    /// `network.mode="enabled"` neither disables the guest network nor starts
    /// a usable box. Reject it and point at the option that does the job.
    #[test]
    fn host_network_grants_off_with_guest_network_on_is_rejected() {
        let mut advanced = AdvancedBoxOptions::default();
        advanced.security = SecurityOptions {
            network_enabled: false,
            ..SecurityOptions::default()
        };
        let opts = BoxOptions {
            advanced,
            ..Default::default()
        };
        assert!(
            matches!(opts.network, NetworkSpec::Enabled { .. }),
            "guard assumes the default network mode is enabled"
        );

        let error = opts.sanitize_common().unwrap_err().to_string();

        assert!(
            error.contains("network.mode=\"disabled\""),
            "error must name the option that disables guest networking: {error}"
        );
        assert!(
            error.contains("advanced.security.network_enabled"),
            "error must name the option the caller actually set: {error}"
        );
    }

    /// The pairing that genuinely means "no network" stays valid, and so does
    /// leaving the host grants at their default.
    #[test]
    fn network_disabled_pairs_with_either_host_grant_setting() {
        for network_enabled in [true, false] {
            let mut advanced = AdvancedBoxOptions::default();
            advanced.security = SecurityOptions {
                network_enabled,
                ..SecurityOptions::default()
            };
            let opts = BoxOptions {
                network: NetworkSpec::Disabled,
                advanced,
                ..Default::default()
            };

            opts.sanitize_common().unwrap_or_else(|e| {
                panic!("network.mode=disabled + network_enabled={network_enabled} rejected: {e}")
            });
        }
    }

    #[test]
    fn custom_kernel_must_be_a_file() {
        let mut advanced = AdvancedBoxOptions::default();
        advanced.kernel = Some(KernelOptions::new("/definitely/missing/vmlinux"));
        let mut opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        let error = opts.sanitize().unwrap_err().to_string();
        assert!(error.contains("custom kernel"), "unexpected error: {error}");
        assert!(error.contains("regular file"), "unexpected error: {error}");
    }

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn persisted_custom_kernel_boot_assets_do_not_require_original_source() {
        #[cfg(target_arch = "x86_64")]
        let format = KernelFormat::Elf;
        #[cfg(target_arch = "aarch64")]
        let format = KernelFormat::PeGz;
        let mut advanced = AdvancedBoxOptions::default();
        advanced.kernel = Some(
            KernelOptions::new("/source/removed/after-create")
                .with_format(format)
                .with_command_line("console=ttyS0"),
        );
        let mut opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        opts.sanitize_persisted().unwrap();
        assert!(opts.sanitize().is_err());
    }

    #[test]
    fn custom_initramfs_must_be_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let kernel = temp.path().join("vmlinux");
        std::fs::write(&kernel, b"\x7fELFcustom kernel").unwrap();
        let mut advanced = AdvancedBoxOptions::default();
        advanced.kernel = Some(
            KernelOptions::new(&kernel)
                .with_format(KernelFormat::Elf)
                .with_initramfs(temp.path().join("missing-initramfs")),
        );
        let mut opts = BoxOptions {
            advanced,
            ..Default::default()
        };

        let error = opts.sanitize().unwrap_err().to_string();
        assert!(error.contains("initramfs"), "unexpected error: {error}");
        assert!(error.contains("regular file"), "unexpected error: {error}");
    }

    #[test]
    #[allow(deprecated)]
    fn explicit_auto_delete_takes_precedence_over_auto_remove() {
        let keep = BoxOptions {
            auto_remove: true,
            auto_delete: Some(0),
            ..Default::default()
        };
        assert!(!keep.removes_on_stop());

        let remove = BoxOptions {
            auto_remove: false,
            auto_delete: Some(60),
            ..Default::default()
        };
        assert!(remove.removes_on_stop());

        let legacy_keep = BoxOptions {
            auto_remove: false,
            auto_delete: None,
            ..Default::default()
        };
        assert!(!legacy_keep.removes_on_stop());
    }

    #[test]
    fn test_box_options_serde_defaults() {
        // Test that serde uses correct defaults for missing fields
        // Must include all required fields that don't have serde defaults
        let json = r#"{
            "rootfs": {"Image": "alpine:latest"},
            "env": [],
            "volumes": [],
            "network": {"Enabled": {"allow_net": []}},
            "ports": []
        }"#;
        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        assert_eq!(opts.auto_delete, None);
        assert!(opts.removes_on_stop());
        assert!(!opts.detach, "detach should default to false via serde");
    }

    #[test]
    fn test_box_options_serde_explicit_values() {
        let json = r#"{
            "rootfs": {"Image": "alpine"},
            "env": [],
            "volumes": [],
            "network": {"Enabled": {"allow_net": []}},
            "ports": [],
            "auto_delete": 0,
            "detach": true
        }"#;
        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        assert_eq!(opts.auto_delete, Some(0));
        assert!(opts.detach, "explicit detach=true should be respected");
    }

    /// The opt-in is persisted with the box and rechecked on every start, so it
    /// has to survive a manifest round-trip — and default to off when absent.
    #[test]
    fn nested_virtualization_option_roundtrips() {
        let stored: BoxOptions =
            serde_json::from_str(r#"{"advanced":{"nested_virtualization":true}}"#).unwrap();
        assert!(stored.advanced.nested_virtualization);
        assert_eq!(
            serde_json::to_value(stored).unwrap()["advanced"]["nested_virtualization"],
            serde_json::Value::Bool(true)
        );

        let legacy: BoxOptions = serde_json::from_str("{}").unwrap();
        assert!(!legacy.advanced.nested_virtualization);
    }

    #[test]
    fn privileged_option_roundtrips() {
        let stored: BoxOptions =
            serde_json::from_str(r#"{"advanced":{"privileged":true}}"#).unwrap();
        assert!(stored.advanced.privileged);
        assert_eq!(
            serde_json::to_value(stored).unwrap()["advanced"]["privileged"],
            serde_json::Value::Bool(true)
        );

        let legacy: BoxOptions = serde_json::from_str("{}").unwrap();
        assert!(!legacy.advanced.privileged);
    }

    #[test]
    fn privileged_rejects_explicit_capability_overrides() {
        let options: BoxOptions = serde_json::from_str(
            r#"{"advanced":{"privileged":true,"capabilities":{"add":["SYS_ADMIN"],"drop":["NET_RAW"]}}}"#,
        )
        .unwrap();

        let error = options
            .advanced
            .validate_privileged_capability_conflict()
            .expect_err("privileged capability overrides must be rejected");

        assert!(error.to_string().contains("cannot be combined"));
    }

    /// The canonical shape is accepted, not rewritten: unlike an earlier
    /// version of this option, `capabilities` is never mutated by
    /// `privileged` — this exists for a box persisted by that earlier
    /// version, whose stored `capabilities` already looks like this.
    #[test]
    fn privileged_canonical_capability_shape_remains_accepted() {
        let options: BoxOptions = serde_json::from_str(
            r#"{"advanced":{"privileged":true,"capabilities":{"add":["ALL"],"drop":[]}}}"#,
        )
        .unwrap();

        options
            .advanced
            .validate_privileged_capability_conflict()
            .expect("canonical privileged shape should be accepted");

        let capabilities = options.advanced.capabilities().expect("capabilities set");
        assert_eq!(capabilities.add, ["ALL"]);
        assert!(capabilities.drop.is_empty());
    }

    #[test]
    fn privileged_security_is_resolved_before_guest_init() {
        let mut advanced = crate::AdvancedBoxOptions::default();
        advanced.privileged = true;
        let options = BoxOptions {
            advanced,
            ..Default::default()
        };

        let resolved = options
            .advanced
            .resolve_container_security()
            .expect("privileged security should resolve");

        assert!(resolved.linux.readonly_paths.is_empty());
        assert!(!resolved.mount.options.contains(&"rro".to_string()));
        assert_eq!(resolved.capabilities.add, ["ALL"]);
        assert!(resolved.capabilities.drop.is_empty());
    }

    #[test]
    fn test_box_options_roundtrip() {
        let opts = BoxOptions {
            auto_delete: Some(0),
            detach: true,
            ..Default::default()
        };

        let json = serde_json::to_string(&opts).unwrap();
        let opts2: BoxOptions = serde_json::from_str(&json).unwrap();

        assert_eq!(opts.auto_delete, opts2.auto_delete);
        assert_eq!(opts.detach, opts2.detach);
    }

    #[test]
    fn test_network_mode_from_str() {
        assert_eq!(
            "enabled".parse::<NetworkMode>().unwrap(),
            NetworkMode::Enabled
        );
        assert_eq!(
            "disabled".parse::<NetworkMode>().unwrap(),
            NetworkMode::Disabled
        );
    }

    #[test]
    fn test_network_mode_from_str_rejects_invalid_values() {
        let err = "broken".parse::<NetworkMode>().unwrap_err().to_string();
        assert!(err.contains("invalid network mode"));
    }

    #[test]
    fn test_network_config_enabled_converts_to_internal_network_spec() {
        let spec = NetworkSpec::try_from(OutboundNetworkConfig {
            mode: NetworkMode::Enabled,
            allow_net: vec!["example.com".to_string()],
        })
        .unwrap();

        match spec {
            NetworkSpec::Enabled { allow_net } => {
                assert_eq!(allow_net, vec!["example.com".to_string()]);
            }
            NetworkSpec::Disabled => panic!("expected enabled network spec"),
        }
    }

    #[test]
    fn test_inbound_config_converts_to_internal_spec() {
        let spec = NetworkSpec::try_from(InboundNetworkConfig {
            mode: NetworkMode::Disabled,
            allow_net: Vec::new(),
        })
        .unwrap();
        assert!(matches!(spec, NetworkSpec::Disabled));
    }

    #[test]
    fn test_network_config_rejects_unsupported_inbound_allow_net() {
        // No layer enforces an inbound allowlist yet; a non-empty one is
        // rejected outright rather than accepted as a silent no-op.
        let err = NetworkSpec::try_from(InboundNetworkConfig {
            mode: NetworkMode::Enabled,
            allow_net: vec!["10.0.0.0/8".to_string()],
        })
        .unwrap_err();
        assert!(err.to_string().contains("not supported yet"));
        // POL-356: this is the caller's mistake, not the server's — over
        // boxlite serve it must reach the client as a 400, not a 500. The
        // variant is what BoxliteError::http() dispatches on, so pin it here
        // rather than only the message.
        assert!(
            matches!(
                err,
                boxlite_shared::errors::BoxliteError::InvalidArgument(_)
            ),
            "expected InvalidArgument (→ HTTP 400), got {err:?}"
        );
    }

    #[test]
    fn test_sanitize_rejects_unsupported_inbound_allow_net() {
        // FFI callers (C/Go) set the spec directly, bypassing try_from —
        // sanitize() is the create-time backstop for those paths.
        let mut opts = BoxOptions {
            inbound_network: NetworkSpec::Enabled {
                allow_net: vec!["10.0.0.0/8".to_string()],
            },
            ..Default::default()
        };
        let err = opts.sanitize().unwrap_err();
        assert!(err.to_string().contains("not supported yet"));
        // POL-356: same HTTP-mapping requirement as the try_from path above —
        // this backstop must not silently regress to a 500.
        assert!(
            matches!(
                err,
                boxlite_shared::errors::BoxliteError::InvalidArgument(_)
            ),
            "expected InvalidArgument (→ HTTP 400), got {err:?}"
        );
    }

    /// `VolumeSpec::managed_volume` and `VolumeSpec::bind_mount` cannot produce a mount
    /// with two origins or none, but a struct literal and the C/Go FFI both
    /// can, so sanitize() is the create-time backstop for those paths.
    #[test]
    fn test_sanitize_rejects_ambiguous_volume_origin() {
        let mut both = BoxOptions {
            volumes: vec![VolumeSpec {
                managed_volume: Some("my-data".into()),
                host_path: "/tmp/data".into(),
                guest_path: "/data".into(),
                read_only: false,
            }],
            ..Default::default()
        };
        let err = both.sanitize().unwrap_err().to_string();
        assert!(err.contains("exactly one"), "{err}");

        let mut neither = BoxOptions {
            volumes: vec![VolumeSpec {
                guest_path: "/data".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let err = neither.sanitize().unwrap_err().to_string();
        assert!(err.contains("volume id or name"), "{err}");

        let mut empty_reference = BoxOptions {
            volumes: vec![VolumeSpec::managed_volume("   ", "/data")],
            ..Default::default()
        };
        let err = empty_reference.sanitize().unwrap_err().to_string();
        assert!(err.contains("empty managed_volume"), "{err}");
    }

    /// A box persisted before `managed_volume` existed carries only
    /// `host_path`; it must still load, as a host bind, without a migration.
    #[test]
    fn legacy_volume_json_without_managed_volume_still_loads() {
        let mut opts: BoxOptions = serde_json::from_str(
            r#"{"volumes":[{"host_path":"/tmp/data","guest_path":"/data","read_only":true}]}"#,
        )
        .expect("pre-managed_volume box config must still deserialize");

        let volume = &opts.volumes[0];
        assert_eq!(volume.managed_volume, None);
        assert_eq!(volume.host_path, "/tmp/data");
        assert!(volume.read_only);
        opts.sanitize().expect("a legacy host bind is still valid");
    }

    /// An ordinary host bind must not start writing a `managed_volume` key
    /// into persisted configs and archives — an older reader has no field for
    /// it. Same reasoning as `box_options_omits_capabilities_key_when_unspecified`.
    #[test]
    fn host_volume_omits_managed_volume_key() {
        let opts = BoxOptions {
            volumes: vec![VolumeSpec::bind_mount("/tmp/data", "/data")],
            ..Default::default()
        };

        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&opts).unwrap()).unwrap();
        let volume = &json["volumes"][0];

        assert!(
            !volume.as_object().unwrap().contains_key("managed_volume"),
            "a host bind must omit the key, not serialize null: {volume}"
        );
    }

    #[test]
    fn test_network_config_disabled_rejects_allow_net() {
        let err = NetworkSpec::try_from(OutboundNetworkConfig {
            mode: NetworkMode::Disabled,
            allow_net: vec!["example.com".to_string()],
        })
        .unwrap_err()
        .to_string();

        assert!(err.contains("network.outbound.mode=\"disabled\""));
    }

    #[test]
    fn test_network_spec_converts_to_public_network_config() {
        let config = NetworkConfig::from_specs(
            &NetworkSpec::Disabled,
            &NetworkSpec::Enabled {
                allow_net: Vec::new(),
            },
        );
        assert_eq!(config.outbound.mode, NetworkMode::Disabled);
        assert!(config.outbound.allow_net.is_empty());
        assert_eq!(config.inbound.mode, NetworkMode::Enabled);
    }

    #[test]
    fn test_sanitize_remove_on_stop_detach_incompatible() {
        let mut opts = BoxOptions {
            auto_delete: Some(1),
            detach: true,
            ..Default::default()
        };
        let err_msg = opts.sanitize().unwrap_err().to_string();
        assert!(err_msg.contains("incompatible"));
    }

    #[test]
    fn test_sanitize_valid_combinations() {
        let mut remove = BoxOptions {
            auto_delete: Some(1),
            ..Default::default()
        };
        assert!(remove.sanitize().is_ok());

        let mut keep_detached = BoxOptions {
            auto_delete: Some(0),
            detach: true,
            ..Default::default()
        };
        assert!(keep_detached.sanitize().is_ok());

        let mut keep_attached = BoxOptions {
            auto_delete: Some(0),
            ..Default::default()
        };
        assert!(keep_attached.sanitize().is_ok());
    }

    #[test]
    fn test_sanitize_allows_duplicate_automatic_ports() {
        let duplicate = PortSpec {
            host_port: None,
            guest_port: 3000,
            protocol: PortProtocol::Tcp,
            host_ip: None,
        };
        let mut opts = BoxOptions {
            ports: vec![
                duplicate.clone(),
                PortSpec {
                    host_port: Some(0),
                    host_ip: Some("0.0.0.0".to_string()),
                    ..duplicate
                },
            ],
            ..Default::default()
        };

        assert!(opts.sanitize().is_ok());
    }

    // ========================================================================
    // SecurityOptionsBuilder tests
    // ========================================================================

    #[test]
    fn test_security_builder_new() {
        let opts = SecurityOptionsBuilder::new().build();
        // Default is now the standard preset on both Linux and macOS
        // (flipped in this PR — previously Linux defaulted off, which
        // meant REST / CLI / JSON-config paths silently ran unsandboxed).
        //   - jailer enabled on Linux + macOS
        //   - seccomp enabled on Linux (no-op on macOS)
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(opts.jailer_enabled);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert!(!opts.jailer_enabled);
        #[cfg(target_os = "linux")]
        assert!(opts.seccomp_enabled);
        #[cfg(not(target_os = "linux"))]
        assert!(!opts.seccomp_enabled);
    }

    #[test]
    fn test_security_builder_presets() {
        // Two settings only: enabled (full) and disabled (master switch off,
        // every sub-protection off).
        let off = SecurityOptionsBuilder::disabled().build();
        assert!(!off.jailer_enabled);
        assert!(!off.close_fds);
        assert!(!off.sanitize_env);
        assert!(off.uid.is_none());

        let on = SecurityOptionsBuilder::enabled().build();
        assert!(on.jailer_enabled);
        assert!(on.close_fds);
        assert!(on.sanitize_env);
        assert_eq!(on, SecurityOptions::default(), "enabled is the default");
    }

    // Single source of truth for the default profile: deserializing an empty
    // object must yield exactly `SecurityOptions::default()`. This guards the
    // struct-level `#[serde(default)]`. Previously each field carried its own
    // `#[serde(default = "...")]` that diverged from `Default` — a partial JSON
    // body (e.g. a `{}` security block) silently produced a *weaker* sandbox
    // (uid unset, no resource limits, no new PID ns on Linux). Reintroducing
    // per-field serde defaults that disagree with `Default` flips this red.
    #[test]
    fn deserializing_empty_equals_default() {
        let from_json: SecurityOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(from_json, SecurityOptions::default());
    }

    // ===========================================================
    // SecurityOptions::from_preset — operator-surface contract
    //
    // CLI / REST / Go / C all funnel the setting *string* through this
    // helper. Reverting (deleting the match) flips all four red. There
    // are two settings — enable (default) and disable — each with
    // documented synonyms (on/off).
    // ===========================================================

    #[test]
    fn security_from_preset_canonical_names() {
        use crate::runtime::advanced_options::SecurityOptions;
        assert_eq!(
            SecurityOptions::from_preset("enable").unwrap(),
            SecurityOptions::enabled()
        );
        assert_eq!(
            SecurityOptions::from_preset("disable").unwrap(),
            SecurityOptions::disabled()
        );
    }

    #[test]
    fn security_from_preset_case_insensitive_and_synonyms() {
        use crate::runtime::advanced_options::SecurityOptions;
        // Casing + whitespace.
        assert_eq!(
            SecurityOptions::from_preset("  ENABLE ").unwrap(),
            SecurityOptions::enabled()
        );
        // Documented synonyms.
        assert_eq!(
            SecurityOptions::from_preset("enabled").unwrap(),
            SecurityOptions::enabled()
        );
        assert_eq!(
            SecurityOptions::from_preset("on").unwrap(),
            SecurityOptions::enabled()
        );
        assert_eq!(
            SecurityOptions::from_preset("disabled").unwrap(),
            SecurityOptions::disabled()
        );
        assert_eq!(
            SecurityOptions::from_preset("off").unwrap(),
            SecurityOptions::disabled()
        );
    }

    #[test]
    fn security_from_preset_unknown_surfaces_invalid_argument() {
        use crate::runtime::advanced_options::SecurityOptions;
        // A previously-valid 3-tier name must now be rejected too.
        let err = SecurityOptions::from_preset("maximum").expect_err("old preset must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("maximum"),
            "rejection must echo the offending value; got {msg}"
        );
        assert!(
            msg.contains("enable") && msg.contains("disable"),
            "rejection must list the supported settings; got {msg}"
        );
    }

    /// Default contract: `SecurityOptions::default()` and
    /// `BoxOptions::default().advanced.security` are the fully-**enabled**
    /// profile. Reverting `Default` to the old moderate/jailer-off value flips
    /// this red.
    #[test]
    fn security_default_is_enabled() {
        use crate::runtime::advanced_options::SecurityOptions;
        let direct = SecurityOptions::default();
        let via_box = BoxOptions::default().advanced.security;
        assert_eq!(direct, SecurityOptions::enabled());
        assert_eq!(via_box, SecurityOptions::enabled());
        // Full profile: jailer master switch + fd/env hardening always on.
        assert!(direct.jailer_enabled);
        assert!(direct.close_fds);
        assert!(direct.sanitize_env);
        assert_eq!(direct.uid, Some(65534));
        #[cfg(target_os = "linux")]
        {
            assert!(direct.seccomp_enabled);
            assert!(direct.new_pid_ns);
            assert!(direct.chroot_enabled);
        }
    }

    #[test]
    fn test_security_builder_chaining() {
        let opts = SecurityOptionsBuilder::enabled()
            .jailer_enabled(true)
            .seccomp_enabled(false)
            .max_open_files(2048)
            .max_processes(50)
            .build();

        assert!(opts.jailer_enabled);
        assert!(!opts.seccomp_enabled);
        assert_eq!(opts.resource_limits.max_open_files, Some(2048));
        assert_eq!(opts.resource_limits.max_processes, Some(50));
    }

    #[test]
    fn test_security_builder_resource_limits() {
        let opts = SecurityOptionsBuilder::new()
            .max_open_files(1024)
            .max_file_size_bytes(1024 * 1024)
            .max_processes(100)
            .max_memory_bytes(512 * 1024 * 1024)
            .max_cpu_time_seconds(300)
            .build();

        assert_eq!(opts.resource_limits.max_open_files, Some(1024));
        assert_eq!(opts.resource_limits.max_file_size, Some(1024 * 1024));
        assert_eq!(opts.resource_limits.max_processes, Some(100));
        assert_eq!(opts.resource_limits.max_memory, Some(512 * 1024 * 1024));
        assert_eq!(opts.resource_limits.max_cpu_time, Some(300));
    }

    #[test]
    fn test_security_builder_env_allowlist() {
        let opts = SecurityOptionsBuilder::new()
            .env_allowlist(vec!["FOO".to_string()])
            .allow_env("BAR")
            .allow_env("BAZ")
            .build();

        assert_eq!(opts.env_allowlist.len(), 3);
        assert!(opts.env_allowlist.contains(&"FOO".to_string()));
        assert!(opts.env_allowlist.contains(&"BAR".to_string()));
        assert!(opts.env_allowlist.contains(&"BAZ".to_string()));
    }

    #[test]
    fn test_security_builder_via_security_options() {
        // Test the convenience method on SecurityOptions
        let opts = SecurityOptions::builder().jailer_enabled(true).build();

        assert!(opts.jailer_enabled);
    }

    // ========================================================================
    // cmd/user option tests
    // ========================================================================

    #[test]
    fn test_box_options_cmd_default_is_none() {
        let opts = BoxOptions::default();
        assert!(opts.cmd.is_none());
    }

    #[test]
    fn test_box_options_user_default_is_none() {
        let opts = BoxOptions::default();
        assert!(opts.user.is_none());
    }

    #[test]
    fn test_box_options_cmd_serde_roundtrip() {
        let opts = BoxOptions {
            cmd: Some(vec!["--flag".to_string(), "value".to_string()]),
            user: Some("1000:1000".to_string()),
            ..Default::default()
        };

        let json = serde_json::to_string(&opts).unwrap();
        let opts2: BoxOptions = serde_json::from_str(&json).unwrap();

        assert_eq!(
            opts2.cmd,
            Some(vec!["--flag".to_string(), "value".to_string()])
        );
        assert_eq!(opts2.user, Some("1000:1000".to_string()));
    }

    #[test]
    fn test_box_options_cmd_serde_missing_defaults_to_none() {
        let json = r#"{
            "rootfs": {"Image": "alpine:latest"},
            "env": [],
            "volumes": [],
            "network": {"Enabled": {"allow_net": []}},
            "ports": []
        }"#;
        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        assert!(
            opts.cmd.is_none(),
            "cmd should default to None when missing from JSON"
        );
        assert!(
            opts.user.is_none(),
            "user should default to None when missing from JSON"
        );
    }

    #[test]
    fn test_box_options_cmd_explicit_in_json() {
        let json = r#"{
            "rootfs": {"Image": "docker:dind"},
            "env": [],
            "volumes": [],
            "network": {"Enabled": {"allow_net": []}},
            "ports": [],
            "cmd": ["--iptables=false"],
            "user": "1000:1000"
        }"#;
        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        assert_eq!(opts.cmd, Some(vec!["--iptables=false".to_string()]));
        assert_eq!(opts.user, Some("1000:1000".to_string()));
    }

    #[test]
    fn test_box_options_entrypoint_default_is_none() {
        let opts = BoxOptions::default();
        assert!(opts.entrypoint.is_none());
    }

    #[test]
    fn test_box_options_entrypoint_serde_roundtrip() {
        let opts = BoxOptions {
            entrypoint: Some(vec!["dockerd".to_string()]),
            cmd: Some(vec!["--iptables=false".to_string()]),
            ..Default::default()
        };

        let json = serde_json::to_string(&opts).unwrap();
        let opts2: BoxOptions = serde_json::from_str(&json).unwrap();

        assert_eq!(opts2.entrypoint, Some(vec!["dockerd".to_string()]));
        assert_eq!(opts2.cmd, Some(vec!["--iptables=false".to_string()]));
    }

    #[test]
    fn test_box_options_entrypoint_missing_defaults_to_none() {
        let json = r#"{
            "rootfs": {"Image": "alpine:latest"},
            "env": [],
            "volumes": [],
            "network": {"Enabled": {"allow_net": []}},
            "ports": []
        }"#;
        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        assert!(
            opts.entrypoint.is_none(),
            "entrypoint should default to None when missing from JSON"
        );
    }

    #[test]
    fn test_box_options_entrypoint_explicit_in_json() {
        let json = r#"{
            "rootfs": {"Image": "docker:dind"},
            "env": [],
            "volumes": [],
            "network": {"Enabled": {"allow_net": []}},
            "ports": [],
            "entrypoint": ["dockerd"],
            "cmd": ["--iptables=false"]
        }"#;
        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        assert_eq!(opts.entrypoint, Some(vec!["dockerd".to_string()]));
        assert_eq!(opts.cmd, Some(vec!["--iptables=false".to_string()]));
    }

    // ========================================================================
    // Secret tests
    // ========================================================================

    fn test_secret() -> Secret {
        Secret {
            name: "openai".to_string(),
            hosts: vec!["api.openai.com".to_string()],
            placeholder: "<BOXLITE_SECRET:openai>".to_string(),
            value: "sk-test-super-secret-key-12345".to_string(),
        }
    }

    #[test]
    fn test_secret_serde_roundtrip() {
        let secret = test_secret();
        let json = serde_json::to_string(&secret).unwrap();
        let deserialized: Secret = serde_json::from_str(&json).unwrap();
        assert_eq!(secret, deserialized);
        // Value IS serialized (needed for DB persistence)
        assert!(json.contains("sk-test-super-secret-key-12345"));
    }

    #[test]
    fn test_secret_env_key_valid_names() {
        let cases = [
            ("openai", "BOXLITE_SECRET_OPENAI"),
            ("my_key", "BOXLITE_SECRET_MY_KEY"),
            ("KEY123", "BOXLITE_SECRET_KEY123"),
            ("a-b-c", "BOXLITE_SECRET_A_B_C"), // hyphen → underscore
        ];
        for (name, expected) in cases {
            let secret = Secret {
                name: name.into(),
                hosts: vec![],
                placeholder: String::new(),
                value: String::new(),
            };
            assert_eq!(secret.env_key(), expected, "name={name:?}");
        }
    }

    #[test]
    fn test_secret_env_key_sanitizes_invalid_names() {
        let cases = [
            ("my key", "BOXLITE_SECRET_MY_KEY"), // space → _
            ("a/b/c", "BOXLITE_SECRET_A_B_C"),   // slash → _
            ("", "BOXLITE_SECRET__UNNAMED"),     // empty
            ("café", "BOXLITE_SECRET_CAF_"),     // non-ascii → _
        ];
        for (name, expected) in cases {
            let secret = Secret {
                name: name.into(),
                hosts: vec![],
                placeholder: String::new(),
                value: String::new(),
            };
            assert_eq!(secret.env_key(), expected, "name={name:?}");
        }
    }

    #[test]
    fn test_secret_debug_redacts_value() {
        let secret = test_secret();
        let debug_output = format!("{:?}", secret);
        assert!(
            !debug_output.contains("sk-test-super-secret-key-12345"),
            "Debug output must not contain the secret value"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]"
        );
        assert!(
            debug_output.contains("openai"),
            "Debug output should contain the secret name"
        );
    }

    #[test]
    fn test_secret_display_redacts_value() {
        let secret = test_secret();
        let display_output = format!("{}", secret);
        assert!(
            !display_output.contains("sk-test-super-secret-key-12345"),
            "Display output must not contain the secret value"
        );
        assert!(
            display_output.contains("[REDACTED]"),
            "Display output must contain [REDACTED]"
        );
    }

    #[test]
    fn test_secret_serde_json_fields() {
        let secret = test_secret();
        let value = serde_json::to_value(&secret).unwrap();
        assert!(value.get("name").unwrap().is_string());
        assert!(value.get("hosts").unwrap().is_array());
        assert!(value.get("placeholder").unwrap().is_string());
        assert!(value.get("value").unwrap().is_string());
        assert_eq!(value.get("hosts").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_box_options_with_secrets_default() {
        let opts = BoxOptions::default();
        assert!(opts.secrets.is_empty(), "secrets should default to empty");
    }

    #[test]
    fn test_box_options_with_secrets_serde() {
        let opts = BoxOptions {
            secrets: vec![test_secret()],
            ..Default::default()
        };
        let json = serde_json::to_string(&opts).unwrap();
        let deserialized: BoxOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.secrets.len(), 1);
        assert_eq!(deserialized.secrets[0], test_secret());
    }

    #[test]
    fn test_box_options_secrets_in_json() {
        let opts = BoxOptions {
            secrets: vec![
                test_secret(),
                Secret {
                    name: "anthropic".to_string(),
                    hosts: vec!["api.anthropic.com".to_string()],
                    placeholder: "<BOXLITE_SECRET:anthropic>".to_string(),
                    value: "sk-ant-secret".to_string(),
                },
            ],
            ..Default::default()
        };
        let json = serde_json::to_string(&opts).unwrap();
        assert!(
            json.contains("\"secrets\""),
            "JSON must contain secrets key"
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let secrets_arr = value.get("secrets").unwrap().as_array().unwrap();
        assert_eq!(secrets_arr.len(), 2);
    }

    #[test]
    fn test_box_options_secrets_missing_from_json_defaults_empty() {
        let json = r#"{
            "rootfs": {"Image": "alpine:latest"},
            "env": [],
            "volumes": [],
            "network": {"Enabled": {"allow_net": []}},
            "ports": []
        }"#;
        let opts: BoxOptions = serde_json::from_str(json).unwrap();
        assert!(
            opts.secrets.is_empty(),
            "secrets should default to empty when missing from JSON"
        );
    }

    #[test]
    fn test_security_builder_non_consuming() {
        // Verify builder can be reused (non-consuming pattern). Start from the
        // disabled profile so resource limits begin unset and the assertions
        // below isolate exactly what each `build()` added.
        let mut builder = SecurityOptionsBuilder::disabled();
        builder.max_open_files(1024);

        let opts1 = builder.build();
        let opts2 = builder.max_processes(50).build();

        // Both should have max_open_files
        assert_eq!(opts1.resource_limits.max_open_files, Some(1024));
        assert_eq!(opts2.resource_limits.max_open_files, Some(1024));

        // Only opts2 should have max_processes
        assert!(opts1.resource_limits.max_processes.is_none());
        assert_eq!(opts2.resource_limits.max_processes, Some(50));
    }

    /// The whole point of #1152: the ceiling has to follow the box's own
    /// `--disk-size`, not a constant picked before the box existed.
    #[test]
    fn sanitize_scales_fsize_with_the_requested_disk() {
        let mut options = BoxOptions {
            disk_size_gb: Some(20),
            ..BoxOptions::default()
        };
        options.sanitize().unwrap();

        assert_eq!(
            options.advanced.security.resource_limits.max_file_size,
            Some(Bytes::from_gib(40).as_bytes()),
            "a 20 GiB box gets a 40 GiB ceiling"
        );
    }

    /// No explicit size: the disk is created at `DEFAULT_DISK_SIZE_GB`, so the
    /// ceiling tracks that.
    #[test]
    fn sanitize_falls_back_to_the_default_disk_size() {
        let mut options = BoxOptions::default();
        options.sanitize().unwrap();

        assert_eq!(
            options.advanced.security.resource_limits.max_file_size,
            Some(Bytes::from_gib(20).as_bytes()),
            "the default 10 GiB disk gets a 20 GiB ceiling"
        );
    }
}
