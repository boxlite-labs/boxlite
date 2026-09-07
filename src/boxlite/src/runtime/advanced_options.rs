//! Advanced options for expert users.
//!
//! This module contains [`AdvancedBoxOptions`], [`ContainerCapabilities`],
//! [`SecurityOptions`], [`ResourceLimits`], and [`SecurityOptionsBuilder`] —
//! configuration that entry-level users can safely ignore. Defaults prioritize
//! compatibility. Direct custom-kernel boot is also grouped here because it
//! changes the VM boot contract.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

// ============================================================================
// Health Check Options
// ============================================================================

/// Health check options for boxes.
///
/// Defines how to periodically check if a box's guest agent is responsive.
/// Similar to Docker's HEALTHCHECK directive.
///
/// This is an advanced option - most users should rely on the defaults.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct HealthCheckOptions {
    /// Time between health checks.
    ///
    /// Default: 30 seconds
    #[serde(default = "default_health_interval")]
    pub interval: Duration,

    /// Time to wait before considering the check failed.
    ///
    /// Default: 10 seconds
    #[serde(default = "default_health_timeout")]
    pub timeout: Duration,

    /// Number of consecutive failures before marking as unhealthy.
    ///
    /// Default: 3
    #[serde(default = "default_health_retries")]
    pub retries: u32,

    /// Startup period before health checks count toward failures.
    ///
    /// During this period, failures don't count toward the retry limit.
    /// This gives the box time to boot up before being marked unhealthy.
    ///
    /// Default: 60 seconds
    #[serde(default = "default_health_start_period")]
    pub start_period: Duration,
}

fn default_health_interval() -> Duration {
    Duration::from_secs(30)
}

fn default_health_timeout() -> Duration {
    Duration::from_secs(10)
}

fn default_health_retries() -> u32 {
    3
}

fn default_health_start_period() -> Duration {
    Duration::from_secs(60)
}

impl Default for HealthCheckOptions {
    fn default() -> Self {
        Self {
            interval: default_health_interval(),
            timeout: default_health_timeout(),
            retries: default_health_retries(),
            start_period: default_health_start_period(),
        }
    }
}

// ============================================================================
// Security Options
// ============================================================================

/// Security isolation options for a box.
///
/// These options control how the boxlite-shim process is isolated from the host.
/// Different presets are available for different security requirements.
/// `#[serde(default)]` is at the struct level on purpose: any field missing
/// from the input falls back to `SecurityOptions::default()`, so deserializing
/// `{}` is identical to `SecurityOptions::default()`. There is exactly one
/// source of truth for "the default profile" — the `Default` impl below — and
/// `deserializing_empty_equals_default` pins it. (Previously each field carried
/// its own `#[serde(default = "...")]`, which silently diverged from `Default`
/// and let a partial JSON body land a *weaker* sandbox than `default()`.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecurityOptions {
    /// Enable jailer isolation.
    ///
    /// When true, applies platform-specific security isolation:
    /// - Linux: seccomp, namespaces, chroot, privilege drop
    /// - macOS: sandbox-exec profile
    ///
    /// Default: on for Linux and macOS (see `SecurityOptions::default`).
    pub jailer_enabled: bool,

    /// Enable seccomp syscall filtering (Linux only).
    ///
    /// When true, applies a whitelist of allowed syscalls.
    pub seccomp_enabled: bool,

    /// UID to drop to after setup (Linux only).
    ///
    /// - None: Auto-allocate an unprivileged UID
    /// - Some(0): Don't drop privileges (not recommended)
    /// - Some(uid): Drop to specific UID
    pub uid: Option<u32>,

    /// GID to drop to after setup (Linux only).
    ///
    /// - None: Auto-allocate an unprivileged GID
    /// - Some(0): Don't drop privileges (not recommended)
    /// - Some(gid): Drop to specific GID
    pub gid: Option<u32>,

    /// Create new PID namespace (Linux only).
    ///
    /// When true, the shim becomes PID 1 in a new namespace.
    pub new_pid_ns: bool,

    /// Create new network namespace (Linux only).
    ///
    /// When true, creates isolated network namespace.
    /// Note: gvproxy handles networking, so this may not be needed.
    pub new_net_ns: bool,

    /// Base directory for chroot jails (Linux only).
    ///
    /// Default: /srv/boxlite
    pub chroot_base: PathBuf,

    /// Enable chroot isolation (Linux only).
    ///
    /// When true, uses pivot_root to isolate filesystem.
    pub chroot_enabled: bool,

    /// Close inherited file descriptors.
    ///
    /// When true, closes all FDs except stdin/stdout/stderr before VM start.
    pub close_fds: bool,

    /// Sanitize environment variables.
    ///
    /// When true, clears all environment variables except those in allowlist.
    pub sanitize_env: bool,

    /// Environment variables to preserve when sanitizing.
    ///
    /// See `SecurityOptions::default` for the default allowlist.
    pub env_allowlist: Vec<String>,

    /// Resource limits to apply.
    pub resource_limits: ResourceLimits,

    /// Custom sandbox profile path (macOS only).
    ///
    /// If None, uses the built-in modular sandbox profile.
    pub sandbox_profile: Option<PathBuf>,

    /// Allow host-side IP networking grants in the jailer profile.
    ///
    /// This is not the guest-networking switch; use `network.mode` for that.
    /// The shim's AF_UNIX control plane is granted separately.
    ///
    /// Allow a box to start when the host cannot enforce its per-box cgroup
    /// limits.
    ///
    /// `THREAT_MODEL.md` lists resource fairness — "one guest cannot starve
    /// others", enforced by cgroups and rlimits — under *Guaranteed*
    /// properties, not best-effort. A guaranteed property must not degrade
    /// silently, so the default is to refuse the box rather than run it
    /// without a ceiling. This mirrors the bwrap user-namespace preflight,
    /// which fails closed and points the operator at
    /// `SecurityOptions::disabled()`.
    ///
    /// Set this only where the limits genuinely cannot be had and an
    /// unconfined box is acceptable — a headless container with no systemd, a
    /// CI image with no `busctl`. Development and CI only: it turns an
    /// enforced ceiling into a logged warning.
    ///
    /// Default: false.
    pub allow_unlimited_host_resources: bool,

    /// Default: true (needed for gvproxy VM networking).
    pub network_enabled: bool,
}

/// Resource limits for the jailed process.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum number of open file descriptors (RLIMIT_NOFILE).
    #[serde(default)]
    pub max_open_files: Option<u64>,

    /// Maximum file size in bytes (RLIMIT_FSIZE).
    #[serde(default)]
    pub max_file_size: Option<u64>,

    /// Maximum number of processes (RLIMIT_NPROC).
    #[serde(default)]
    pub max_processes: Option<u64>,

    /// Maximum virtual memory in bytes (RLIMIT_AS).
    #[serde(default)]
    pub max_memory: Option<u64>,

    /// Maximum CPU time in seconds (RLIMIT_CPU).
    #[serde(default)]
    pub max_cpu_time: Option<u64>,
}

// Internal helpers shared by `Default` and `disabled()`. The per-field serde
// defaults were removed in favour of the struct-level `#[serde(default)]`, so
// `Default` (below) is now the single source of truth for the default profile.

fn default_chroot_base() -> PathBuf {
    PathBuf::from("/srv/boxlite")
}

fn default_network_enabled() -> bool {
    true
}

impl Default for SecurityOptions {
    /// Default is the fully-enabled profile: secure by default.
    /// `enabled()` and `disabled()` are the two named starting profiles;
    /// callers needing something in between override individual fields (or use
    /// the builder / per-field FFI setters) on top of a profile.
    fn default() -> Self {
        Self {
            jailer_enabled: true,
            seccomp_enabled: cfg!(target_os = "linux"),
            uid: Some(65534), // nobody
            gid: Some(65534), // nogroup
            new_pid_ns: cfg!(target_os = "linux"),
            new_net_ns: false, // gvproxy provides networking
            chroot_base: default_chroot_base(),
            chroot_enabled: cfg!(target_os = "linux"),
            close_fds: true,
            sanitize_env: true,
            env_allowlist: vec!["RUST_LOG".to_string()],
            resource_limits: ResourceLimits {
                max_open_files: Some(1024),
                max_file_size: Some(1024 * 1024 * 1024), // 1GB
                // Per-box cgroup `pids.max` — the fork-bomb cap for the box's
                // host-side process tree (bwrap + shim + libkrun/gvproxy
                // threads), which sits well under this. Deliberately does NOT
                // set RLIMIT_NPROC anymore: that is per-host-UID and broke box
                // spawn on busy hosts (see jailer::common::rlimit::apply_limits_raw).
                max_processes: Some(1024),
                max_memory: None,   // VM config handles this
                max_cpu_time: None, // VM config handles this
            },
            sandbox_profile: None,
            network_enabled: default_network_enabled(),
            allow_unlimited_host_resources: false,
        }
    }
}

impl SecurityOptions {
    /// Enabled ("enable"): full host isolation. This is the default — every
    /// protection the platform supports is on (jailer master switch + seccomp,
    /// chroot, new PID ns on Linux; unprivileged uid/gid; closed fds; sanitized
    /// env; resource limits).
    pub fn enabled() -> Self {
        Self::default()
    }

    /// Disabled: the jailer master switch is off and every sub-protection is
    /// off too. The opt-out for debugging / environments that can't sandbox.
    pub fn disabled() -> Self {
        Self {
            jailer_enabled: false,
            seccomp_enabled: false,
            uid: None,
            gid: None,
            new_pid_ns: false,
            new_net_ns: false,
            chroot_base: default_chroot_base(),
            chroot_enabled: false,
            close_fds: false,
            sanitize_env: false,
            env_allowlist: Vec::new(),
            resource_limits: ResourceLimits::default(),
            sandbox_profile: None,
            network_enabled: default_network_enabled(),
            // `disabled()` means every sub-protection is off, so it must also
            // stop refusing boxes on a host that cannot enforce cgroup limits
            // — otherwise the opt-out would be stricter than the default.
            allow_unlimited_host_resources: true,
        }
    }

    /// Resolve one of the two named profiles by name (case-insensitive). Accepts
    /// `enable`/`enabled`/`on` and `disable`/`disabled`/`off`; anything else is
    /// an `InvalidArgument` so operator surfaces echo the typo back verbatim.
    /// This selects a starting profile; finer customization is done by setting
    /// individual fields on the result.
    pub fn from_preset(name: &str) -> boxlite_shared::errors::BoxliteResult<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "enable" | "enabled" | "on" => Ok(Self::enabled()),
            "disable" | "disabled" | "off" => Ok(Self::disabled()),
            other => Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                format!("unknown security setting {other:?}; expected one of enable|disable"),
            )),
        }
    }

    /// Check if current platform supports full jailer features.
    pub fn is_full_isolation_available() -> bool {
        cfg!(target_os = "linux")
    }

    /// Warn about fields set on this profile that the current platform silently
    /// ignores. The struct is a flat bag mixing Linux-only and macOS-only knobs;
    /// without this, a caller enabling e.g. `seccomp_enabled` on macOS gets no
    /// signal that it did nothing. Called at the jailer apply boundary.
    ///
    /// uid/gid are intentionally not warned on: `default()` sets them on every
    /// platform, so flagging them would fire on the default profile and be noise.
    pub fn warn_inert_fields(&self) {
        #[cfg(not(target_os = "linux"))]
        {
            let mut ignored = Vec::new();
            if self.seccomp_enabled {
                ignored.push("seccomp_enabled");
            }
            if self.new_pid_ns {
                ignored.push("new_pid_ns");
            }
            if self.new_net_ns {
                ignored.push("new_net_ns");
            }
            if self.chroot_enabled {
                ignored.push("chroot_enabled");
            }
            if !ignored.is_empty() {
                tracing::warn!(
                    ?ignored,
                    "SecurityOptions: Linux-only isolation requested but ignored on this non-Linux platform"
                );
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            if self.sandbox_profile.is_some() {
                tracing::warn!(
                    "SecurityOptions: sandbox_profile is macOS-only and ignored on this platform"
                );
            }
        }
    }

    /// Create a builder for customizing security options.
    ///
    /// Starts from `SecurityOptions::default()` (the fully-enabled profile).
    ///
    /// # Example
    ///
    /// ```
    /// use boxlite::runtime::advanced_options::SecurityOptions;
    ///
    /// let security = SecurityOptions::builder()
    ///     .max_open_files(1024)
    ///     .build();
    /// ```
    pub fn builder() -> SecurityOptionsBuilder {
        SecurityOptionsBuilder::new()
    }
}

// ============================================================================
// Security Options Builder (C-BUILDER: Non-consuming builder pattern)
// ============================================================================

/// Builder for customizing [`SecurityOptions`].
///
/// Provides a fluent API for configuring security isolation options.
/// Uses non-consuming methods per Rust API guidelines (C-BUILDER).
///
/// # Example
///
/// ```
/// use boxlite::runtime::advanced_options::SecurityOptionsBuilder;
///
/// let security = SecurityOptionsBuilder::enabled()
///     .max_open_files(2048)
///     .max_file_size_bytes(1024 * 1024 * 512) // 512 MiB
///     .build();
/// ```
#[derive(Debug, Clone)]
pub struct SecurityOptionsBuilder {
    inner: SecurityOptions,
}

impl Default for SecurityOptionsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SecurityOptionsBuilder {
    /// Create a builder starting from default options.
    pub fn new() -> Self {
        Self {
            inner: SecurityOptions::default(),
        }
    }

    /// Create a builder starting from the fully-enabled profile (the default).
    pub fn enabled() -> Self {
        Self {
            inner: SecurityOptions::enabled(),
        }
    }

    /// Create a builder starting from the disabled profile (master switch off,
    /// every sub-protection off).
    pub fn disabled() -> Self {
        Self {
            inner: SecurityOptions::disabled(),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // Core isolation settings
    // ─────────────────────────────────────────────────────────────────────

    /// Enable or disable jailer isolation.
    pub fn jailer_enabled(&mut self, enabled: bool) -> &mut Self {
        self.inner.jailer_enabled = enabled;
        self
    }

    /// Enable or disable seccomp syscall filtering (Linux only).
    pub fn seccomp_enabled(&mut self, enabled: bool) -> &mut Self {
        self.inner.seccomp_enabled = enabled;
        self
    }

    /// Set UID to drop to after setup (Linux only).
    pub fn uid(&mut self, uid: u32) -> &mut Self {
        self.inner.uid = Some(uid);
        self
    }

    /// Set GID to drop to after setup (Linux only).
    pub fn gid(&mut self, gid: u32) -> &mut Self {
        self.inner.gid = Some(gid);
        self
    }

    /// Enable or disable new PID namespace (Linux only).
    pub fn new_pid_ns(&mut self, enabled: bool) -> &mut Self {
        self.inner.new_pid_ns = enabled;
        self
    }

    /// Enable or disable new network namespace (Linux only).
    pub fn new_net_ns(&mut self, enabled: bool) -> &mut Self {
        self.inner.new_net_ns = enabled;
        self
    }

    // ─────────────────────────────────────────────────────────────────────
    // Filesystem isolation
    // ─────────────────────────────────────────────────────────────────────

    /// Set base directory for chroot jails (Linux only).
    pub fn chroot_base(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.inner.chroot_base = path.into();
        self
    }

    /// Enable or disable chroot isolation (Linux only).
    pub fn chroot_enabled(&mut self, enabled: bool) -> &mut Self {
        self.inner.chroot_enabled = enabled;
        self
    }

    /// Enable or disable closing inherited file descriptors.
    pub fn close_fds(&mut self, enabled: bool) -> &mut Self {
        self.inner.close_fds = enabled;
        self
    }

    // ─────────────────────────────────────────────────────────────────────
    // Environment settings
    // ─────────────────────────────────────────────────────────────────────

    /// Enable or disable environment variable sanitization.
    pub fn sanitize_env(&mut self, enabled: bool) -> &mut Self {
        self.inner.sanitize_env = enabled;
        self
    }

    /// Set environment variables to preserve when sanitizing.
    pub fn env_allowlist(&mut self, vars: Vec<String>) -> &mut Self {
        self.inner.env_allowlist = vars;
        self
    }

    /// Add an environment variable to the allowlist.
    pub fn allow_env(&mut self, var: impl Into<String>) -> &mut Self {
        self.inner.env_allowlist.push(var.into());
        self
    }

    // ─────────────────────────────────────────────────────────────────────
    // Resource limits (type-safe setters)
    // ─────────────────────────────────────────────────────────────────────

    /// Set all resource limits at once.
    pub fn resource_limits(&mut self, limits: ResourceLimits) -> &mut Self {
        self.inner.resource_limits = limits;
        self
    }

    /// Set maximum number of open file descriptors.
    pub fn max_open_files(&mut self, limit: u64) -> &mut Self {
        self.inner.resource_limits.max_open_files = Some(limit);
        self
    }

    /// Set maximum file size in bytes.
    pub fn max_file_size_bytes(&mut self, bytes: u64) -> &mut Self {
        self.inner.resource_limits.max_file_size = Some(bytes);
        self
    }

    /// Set maximum number of processes.
    pub fn max_processes(&mut self, limit: u64) -> &mut Self {
        self.inner.resource_limits.max_processes = Some(limit);
        self
    }

    /// Set maximum virtual memory in bytes.
    pub fn max_memory_bytes(&mut self, bytes: u64) -> &mut Self {
        self.inner.resource_limits.max_memory = Some(bytes);
        self
    }

    /// Set maximum CPU time in seconds.
    pub fn max_cpu_time_seconds(&mut self, seconds: u64) -> &mut Self {
        self.inner.resource_limits.max_cpu_time = Some(seconds);
        self
    }

    // ─────────────────────────────────────────────────────────────────────
    // macOS-specific settings
    // ─────────────────────────────────────────────────────────────────────

    /// Set custom sandbox profile path (macOS only).
    pub fn sandbox_profile(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.inner.sandbox_profile = Some(path.into());
        self
    }

    /// Allow or deny host-side IP networking grants in the jailer profile.
    ///
    /// This does not disable guest networking; use `network.mode` for that.
    pub fn network_enabled(&mut self, enabled: bool) -> &mut Self {
        self.inner.network_enabled = enabled;
        self
    }

    // ─────────────────────────────────────────────────────────────────────
    // Build
    // ─────────────────────────────────────────────────────────────────────

    /// Build the configured [`SecurityOptions`].
    pub fn build(&self) -> SecurityOptions {
        self.inner.clone()
    }
}

// ============================================================================
// Advanced Options
// ============================================================================

/// Linux capability policy for the container process.
///
/// Capability names are case-insensitive and may include the `CAP_` prefix.
/// The special value `ALL` is accepted in either list. For named conflicts an
/// explicit addition wins; with `add = ["ALL"]`, named removals win. With
/// `drop = ["ALL"]`, explicit additions form the complete resulting set.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerCapabilities {
    /// Capabilities to add to BoxLite's Docker-compatible baseline.
    pub add: Vec<String>,

    /// Capabilities to remove from the resulting capability set.
    pub drop: Vec<String>,
}

impl ContainerCapabilities {
    /// Whether this policy leaves the default capability set unchanged.
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.drop.is_empty()
    }

    pub(crate) fn is_privileged_capability_shape(&self) -> bool {
        self.drop.is_empty()
            && self.add.len() == 1
            && canonical_capability_name(&self.add[0]) == "ALL"
    }

    pub(crate) fn validate(&self) -> boxlite_shared::errors::BoxliteResult<()> {
        validate_capability_names("advanced.capabilities.add", &self.add)?;
        validate_capability_names("advanced.capabilities.drop", &self.drop)
    }

    /// Check the requested policy against the one recorded for an existing box.
    pub(crate) fn check_compatibility(
        &self,
        actual: &Self,
        box_name: &str,
    ) -> boxlite_shared::errors::BoxliteResult<()> {
        let canonicalize = |capabilities: &[String]| {
            capabilities
                .iter()
                .map(|capability| canonical_capability_name(capability))
                .collect::<std::collections::BTreeSet<_>>()
        };

        if canonicalize(&self.add) == canonicalize(&actual.add)
            && canonicalize(&self.drop) == canonicalize(&actual.drop)
        {
            return Ok(());
        }

        Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
            format!(
                "box '{box_name}' already exists with a different capability policy; reuse it \
                 with the same advanced.capabilities, or create a box under a new name"
            ),
        ))
    }
}

/// Uppercase a capability and strip its optional `CAP_` prefix.
///
/// Validation and reuse comparison share one canonical form so they cannot
/// disagree about whether `net_raw` and `CAP_NET_RAW` are the same policy.
fn canonical_capability_name(capability: &str) -> String {
    let normalized = capability.to_ascii_uppercase();
    normalized
        .strip_prefix("CAP_")
        .unwrap_or(&normalized)
        .to_string()
}

fn validate_capability_names(
    option: &str,
    capabilities: &[String],
) -> boxlite_shared::errors::BoxliteResult<()> {
    for capability in capabilities {
        let name = canonical_capability_name(capability);
        if name == "ALL" {
            continue;
        }

        if name.is_empty() {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                format!("empty Linux capability in {option}"),
            ));
        }
        let mut bytes = name.bytes();
        let starts_with_letter = bytes.next().is_some_and(|byte| byte.is_ascii_uppercase());
        let has_valid_tail =
            bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
        if !starts_with_letter || !has_valid_tail {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                format!("malformed Linux capability in {option}: {capability}"),
            ));
        }
    }

    Ok(())
}

/// Advanced options for expert users.
///
/// Entry-level users can ignore this — the defaults are secure and sensible.
/// Only modify these if you understand the security implications.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AdvancedBoxOptions {
    /// Linux capability policy for the container process.
    ///
    /// `None` means the caller left it unspecified. That's a distinct state
    /// from `Some` of an empty policy: combined with `privileged`, leaving
    /// this unspecified is the one-flag DinD case (see `privileged`'s own
    /// doc comment), while an explicit override — even an empty one — is a
    /// conflict `validate_privileged_capability_conflict` rejects. Private;
    /// read via `capabilities()`, write via `set_capabilities`, which also
    /// enforces that this can't change out from under an already-resolved
    /// request (see `resolved`).
    ///
    /// `skip_serializing_if` on top of `default` is load-bearing, not
    /// cosmetic: without it, `None` serializes as an explicit `"capabilities":
    /// null`, which a pre-Option build's plain `ContainerCapabilities` field
    /// rejects with "invalid type: null, expected struct
    /// ContainerCapabilities" — `#[serde(default)]` alone only covers an
    /// *absent* key. Omitting the key on `None` keeps an ordinary box's
    /// exported manifest byte-for-byte what a pre-#1296 importer already
    /// handles, which is the entire premise `archive_version_for_options`
    /// relies on to leave that case at `ARCHIVE_VERSION` instead of bumping it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<ContainerCapabilities>,

    /// Security isolation options (jailer, seccomp, namespaces, resource limits).
    ///
    /// Secure by default: the default is the fully-enabled profile
    /// (`SecurityOptions::default() == SecurityOptions::enabled()`) — on Linux
    /// that is jailer + seccomp + new PID ns + chroot + unprivileged uid/gid; on
    /// macOS, sandbox-exec. Named profiles:
    /// - `SecurityOptions::enabled()` (== `default()`) — full isolation
    /// - `SecurityOptions::disabled()` — master switch off, all sub-protections off
    ///
    /// For anything in between, override individual fields on top of a profile.
    #[serde(default)]
    pub security: SecurityOptions,

    /// Enable bind mount isolation for the shared mounts directory.
    ///
    /// When true, creates a read-only bind mount from `mounts/` to `shared/`,
    /// preventing the guest from modifying host-prepared files.
    ///
    /// Requires CAP_SYS_ADMIN (privileged) or FUSE (rootless) on Linux.
    /// Defaults to false.
    #[serde(default)]
    pub isolate_mounts: bool,

    /// Health check options.
    ///
    /// When set, a background task will periodically ping the guest agent
    /// to verify the box is healthy. Unhealthy boxes are marked and can
    /// trigger automatic recovery.
    ///
    /// Most users should rely on the defaults.
    #[serde(default)]
    pub health_check: Option<HealthCheckOptions>,

    /// Release-candidate direct Linux boot configuration.
    ///
    /// The runtime must explicitly enable
    /// [`ExperimentalFeature::CustomKernel`](crate::experimental::ExperimentalFeature::CustomKernel).
    #[doc(hidden)]
    #[serde(default)]
    pub kernel: Option<crate::experimental::custom_kernel::KernelOptions>,

    /// Release-candidate nested virtualization.
    ///
    /// The runtime must explicitly enable
    /// [`ExperimentalFeature::NestedVirtualization`](crate::experimental::ExperimentalFeature::NestedVirtualization).
    #[doc(hidden)]
    #[serde(default)]
    pub nested_virtualization: bool,

    /// Docker-style privileged OCI spec shape for DinD.
    ///
    /// Mirrors how moby itself resolves this (`oci/caps.TweakCapabilities`):
    /// privileged short-circuits straight to every capability, and never
    /// writes that result back into `CapAdd`/`CapDrop` — those stay exactly
    /// what the caller set, and the container's *effective* capabilities are
    /// computed fresh from `privileged` + `capabilities` every time a spec is
    /// built. `resolve_container_security` is BoxLite's equivalent of that
    /// computation: `capabilities` here is never mutated by `privileged` in
    /// either direction, so there is no "did privileged install this or did
    /// the caller" ambiguity to track.
    #[serde(default)]
    pub privileged: bool,

    /// Set once `resolve_container_security` has run on this instance.
    /// `set_capabilities` refuses to change the policy afterward — a
    /// resolved request's capabilities shouldn't shift under it before the
    /// resolved value is actually used. `AtomicBool`, not a plain `bool` or
    /// `Cell`, for two reasons: `resolve_container_security` only borrows
    /// `&self` (it's a read-only computation over everything except this
    /// bookkeeping bit), and `AdvancedBoxOptions` crosses `Send`/`Sync`
    /// boundaries (it lives inside `BoxOptions`, held across `.await`
    /// points) where `Cell` doesn't qualify. Not persisted: a freshly
    /// deserialized instance hasn't resolved anything yet in *this*
    /// process, regardless of the box it was loaded for. `Clone` is
    /// implemented manually below because atomics aren't `Clone` — a clone
    /// is a fresh, unresolved copy, which is the right default for building
    /// a new request off of an old one's data.
    #[serde(skip)]
    resolved: std::sync::atomic::AtomicBool,
}

impl Clone for AdvancedBoxOptions {
    fn clone(&self) -> Self {
        Self {
            capabilities: self.capabilities.clone(),
            security: self.security.clone(),
            isolate_mounts: self.isolate_mounts,
            health_check: self.health_check.clone(),
            kernel: self.kernel.clone(),
            nested_virtualization: self.nested_virtualization,
            privileged: self.privileged,
            resolved: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl AdvancedBoxOptions {
    /// The caller's capability policy, if one was configured. `None` means
    /// unspecified — see the field's own doc comment for why that's not the
    /// same as an explicit empty policy.
    pub fn capabilities(&self) -> Option<&ContainerCapabilities> {
        self.capabilities.as_ref()
    }

    /// Replace the capability policy. `None` clears back to "unspecified".
    ///
    /// Errors if this options object has already been resolved (used to
    /// build a box request via `resolve_container_security`) — capabilities
    /// cannot change after that point.
    pub fn set_capabilities(
        &mut self,
        capabilities: Option<ContainerCapabilities>,
    ) -> boxlite_shared::errors::BoxliteResult<()> {
        if self.resolved.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "capabilities cannot be changed after these options have been resolved".to_string(),
            ));
        }
        self.capabilities = capabilities;
        Ok(())
    }

    /// Reject capability overrides that conflict with privileged mode.
    ///
    /// Moby's own `TweakCapabilities` silently ignores `CapAdd`/`CapDrop`
    /// once `--privileged` is set — a real, reported footgun. Failing loudly
    /// here instead means a caller who sets both finds out at request time,
    /// not by wondering later why their capability override had no effect.
    /// `None` (unspecified) is the only value privileged mode tolerates; an
    /// explicit `Some`, even an empty one, is rejected unless it's already
    /// the canonical `add=["ALL"]` shape — that's what a box created under
    /// an earlier version of this option (which mutated `capabilities` in
    /// place) has persisted, and it's the same effective result
    /// `resolve_container_security` would produce anyway.
    pub(crate) fn validate_privileged_capability_conflict(
        &self,
    ) -> boxlite_shared::errors::BoxliteResult<()> {
        if !self.privileged {
            return Ok(());
        }

        match &self.capabilities {
            None => Ok(()),
            Some(caps) if caps.is_privileged_capability_shape() => Ok(()),
            Some(_) => Err(boxlite_shared::errors::BoxliteError::InvalidArgument(
                "privileged mode cannot be combined with an explicit capabilities override, \
                 including an explicitly empty one — leave capabilities unspecified instead"
                    .to_string(),
            )),
        }
    }

    /// Toggle privileged mode.
    ///
    /// A thin wrapper over the `privileged` field itself — kept as a stable
    /// method for existing callers of this crate's public API rather than
    /// requiring a field assignment. It no longer does anything beyond
    /// `self.privileged = enabled`: `capabilities` isn't installed or
    /// withdrawn here (see `resolve_container_security`, which computes the
    /// effective capability set fresh instead).
    pub fn set_privileged(&mut self, enabled: bool) {
        self.privileged = enabled;
    }

    /// The capability policy actually enforced, after moby-style privileged
    /// resolution — not just the caller's raw `capabilities` field.
    ///
    /// Used wherever the *effective* policy matters rather than what happens
    /// to be stored: box-reuse compatibility in particular, where an
    /// already-persisted privileged box's raw field may predate this —  an
    /// earlier version of this option mutated `capabilities` to `add=["ALL"]`
    /// on enable, so that box's raw field looks different from a new
    /// privileged request that leaves `capabilities` empty even though both
    /// resolve to the same thing.
    pub(crate) fn effective_capabilities(&self) -> ContainerCapabilities {
        if self.privileged {
            ContainerCapabilities {
                add: vec!["ALL".to_string()],
                drop: Vec::new(),
            }
        } else {
            self.capabilities.clone().unwrap_or_default()
        }
    }

    /// Resolve the container security request before it crosses into the guest.
    ///
    /// The host owns the public option semantics *and* the literal OCI values
    /// that follow from it — the readonly-path list and the `/sys` bind's
    /// mount options are resolved here, not re-derived by the guest from a
    /// flag (see docs/architecture/privileged-mode-design.md, Trade-offs,
    /// option F). Masked paths are deliberately not part of this: nothing in
    /// DinD reads a masked path, so the guest keeps applying its own oci-spec
    /// default unconditionally, the same way it did before `privileged`
    /// existed — see the Trade-offs note on the finding that motivated
    /// dropping it. Capabilities follow moby's own `TweakCapabilities`:
    /// privileged short-circuits to every capability and `self.capabilities`
    /// is read, never written — call `validate_privileged_capability_conflict`
    /// first so a conflicting explicit override is rejected rather than
    /// silently overridden here.
    pub(crate) fn resolve_container_security(
        &self,
    ) -> boxlite_shared::errors::BoxliteResult<ResolvedContainerSecurityConfig> {
        self.validate_privileged_capability_conflict()?;
        self.resolved
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let capabilities = self.effective_capabilities();

        let readonly_paths = if self.privileged {
            Vec::new()
        } else {
            default_readonly_paths()
        };

        Ok(ResolvedContainerSecurityConfig {
            capabilities,
            linux: ResolvedLinuxSecurity { readonly_paths },
            mount: ResolvedMountSecurity {
                options: mount_options(self.privileged),
            },
        })
    }
}

/// Default OCI readonly-path list for a non-privileged container. Sourced
/// from `oci_spec::runtime::get_default_readonly_paths()` for the same
/// no-drift reason `advanced_options.rs`'s other host-resolved values follow.
fn default_readonly_paths() -> Vec<String> {
    oci_spec::runtime::get_default_readonly_paths()
}

/// Full resolved option list for the guest's `/sys` recursive bind mount.
/// Recursive: `/sys` is an rbind, and OCI's plain `ro` is applied without
/// `AT_RECURSIVE`, which would leave the guest's cgroup2 submount writable
/// inside the container — hence `rro`, not `ro`, for the non-privileged case.
fn mount_options(privileged: bool) -> Vec<String> {
    let mut options: Vec<String> = ["rbind", "nosuid", "noexec", "nodev"]
        .into_iter()
        .map(String::from)
        .collect();
    if !privileged {
        options.push("rro".to_string());
    }
    options
}

/// Atomic container security configuration crossing the host-to-guest
/// boundary. `readonly_paths`/`mount.options` are literal, host-resolved
/// OCI values the guest assigns verbatim — matching how Docker, Podman, and
/// Kata Containers all hand the enforcing side a finished shape rather than a
/// flag to reinterpret (see docs/architecture/privileged-mode-design.md,
/// Trade-offs, option F). `capabilities` is the one exception: only the
/// guest, across the VM boundary, knows its own kernel's capability ceiling,
/// so it stays as add/drop deltas the guest resolves itself.
///
/// No masked-path field, no cgroup namespace, no allow-all device-cgroup
/// rule: all three were tested and found unnecessary for DinD — nothing in
/// the DinD workflow reads a masked path, the guest never enforced a
/// restrictive device-cgroup default in the first place, and `dockerd`
/// tolerated running without a private cgroup namespace view. The guest keeps
/// applying its own oci-spec masked-path default unconditionally.
///
/// `linux`/`mount` are grouped the same way OCI runtime-spec groups the
/// fields they resolve to — parallel top-level fields of one Spec — rather
/// than flattened across a level of structure that doesn't exist in the
/// source concept. Matches the wire shape (`ContainerAdvancedOptions`) it
/// eventually becomes verbatim. `capabilities` stays flat, not nested under
/// a matching `process` field: it predates this grouping (added in #1047,
/// guest version floor 0.9.8) as a plain field on the wire message, and
/// wrapping it now would collide with what already-deployed guests expect
/// there — see `ContainerAdvancedOptions.capabilities` in service.proto.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolvedContainerSecurityConfig {
    pub(crate) capabilities: ContainerCapabilities,
    pub(crate) linux: ResolvedLinuxSecurity,
    pub(crate) mount: ResolvedMountSecurity,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolvedLinuxSecurity {
    pub(crate) readonly_paths: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolvedMountSecurity {
    pub(crate) options: Vec<String>,
}

#[cfg(test)]
mod resolved_security_tests {
    use super::*;

    /// `default_readonly_paths` calls straight into
    /// `oci_spec::runtime::get_default_readonly_paths()` — a caret
    /// dependency, so a semver-compatible release could change what that
    /// returns without BoxLite choosing to. Pinned to the exact list so that
    /// happening is a decision to review, not a silent security-posture
    /// change picked up on the next `cargo update`.
    #[test]
    fn unprivileged_resolves_hardened_path_defaults() {
        let resolved = AdvancedBoxOptions::default()
            .resolve_container_security()
            .expect("default (unprivileged) security should resolve");

        assert_eq!(
            resolved.linux.readonly_paths,
            [
                "/proc/bus",
                "/proc/fs",
                "/proc/irq",
                "/proc/sys",
                "/proc/sysrq-trigger",
            ]
            .map(String::from)
        );
        assert!(resolved.mount.options.contains(&"rro".to_string()));
    }

    #[test]
    fn set_privileged_toggles_the_plain_field() {
        let mut options = AdvancedBoxOptions::default();

        options.set_privileged(true);
        assert!(options.privileged);

        options.set_privileged(false);
        assert!(!options.privileged);
    }

    /// Matches moby's `TweakCapabilities`: `privileged` alone resolves every
    /// capability, the same one-flag DinD enabler Docker's `--privileged`
    /// is — no separate `capabilities.add = ["ALL"]` required.
    #[test]
    fn privileged_resolves_cleared_readonly_paths_and_writable_sys() {
        let mut options = AdvancedBoxOptions::default();
        options.set_privileged(true);

        let resolved = options
            .resolve_container_security()
            .expect("privileged security should resolve");

        assert!(resolved.linux.readonly_paths.is_empty());
        assert!(!resolved.mount.options.contains(&"rro".to_string()));
        assert_eq!(resolved.capabilities.add, ["ALL"]);
    }

    /// Also like moby's `TweakCapabilities`: privileged resolution reads
    /// `capabilities`, it never writes it. Unlike an earlier version of this
    /// option (which mutated `capabilities` in place on enable, then had to
    /// track whether it was safe to take that mutation back on disable),
    /// there is nothing here to withdraw — the field the caller set is
    /// exactly the field they get back.
    #[test]
    fn privileged_resolution_does_not_mutate_capabilities() {
        let options = AdvancedBoxOptions {
            privileged: true,
            ..Default::default()
        };

        options
            .resolve_container_security()
            .expect("privileged security should resolve");

        assert!(options.capabilities.is_none());
    }

    /// Capabilities and the OCI path/mount shape are resolved from the same
    /// `privileged` bool but are otherwise independent knobs: a capability
    /// override alone (no `privileged`) must not relax the hardened paths.
    #[test]
    fn capability_override_without_privileged_keeps_hardened_paths() {
        let options = AdvancedBoxOptions {
            capabilities: Some(ContainerCapabilities {
                add: vec!["SYS_ADMIN".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };

        let resolved = options
            .resolve_container_security()
            .expect("capability-only options should resolve");

        assert!(!resolved.linux.readonly_paths.is_empty());
        assert!(resolved.mount.options.contains(&"rro".to_string()));
        assert_eq!(resolved.capabilities.add, ["SYS_ADMIN"]);
    }

    /// The conflict guard mirrors a real moby footgun (`--privileged` +
    /// `--cap-drop` silently ignores the drop) but fails loudly instead: a
    /// caller combining `privileged` with an explicit, non-canonical
    /// capability override finds out at request time.
    #[test]
    fn privileged_rejects_conflicting_capability_override() {
        let options = AdvancedBoxOptions {
            privileged: true,
            capabilities: Some(ContainerCapabilities {
                add: vec!["SYS_ADMIN".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        };

        let error = options
            .resolve_container_security()
            .expect_err("privileged mode plus an explicit override should be rejected");

        assert!(error.to_string().contains("cannot be combined"));
    }

    /// The conflict guard runs on the final state at `resolve_container_security`
    /// time, not on each setter call — so it doesn't matter which knob a caller
    /// (e.g. CLI flags, or an SDK's builder) happens to set first. Setting an
    /// explicit capability override and only then turning on `privileged` must
    /// be rejected exactly like setting them in the opposite order.
    #[test]
    fn privileged_rejects_conflicting_override_regardless_of_setter_order() {
        let mut options = AdvancedBoxOptions::default();
        options
            .set_capabilities(Some(ContainerCapabilities {
                add: vec!["SYS_ADMIN".to_string()],
                ..Default::default()
            }))
            .unwrap();
        options.set_privileged(true);

        let error = options.resolve_container_security().expect_err(
            "capabilities-then-privileged should be rejected same as the reverse order",
        );

        assert!(error.to_string().contains("cannot be combined"));
    }

    /// The escape hatch must be opt-in: a caller that asked for nothing gets
    /// the guaranteed property, not a box that quietly runs unconfined.
    /// `THREAT_MODEL.md` lists resource fairness under *Guaranteed*, so this
    /// default is the security posture, not a preference.
    #[test]
    fn unlimited_host_resources_is_off_by_default() {
        assert!(
            !SecurityOptions::default().allow_unlimited_host_resources,
            "a default box must refuse to start when its cgroup limits cannot be enforced"
        );
        assert!(
            !SecurityOptions::enabled().allow_unlimited_host_resources,
            "enabled() is the default profile and must agree with it"
        );
    }

    /// `disabled()` turns every sub-protection off, so it has to turn this one
    /// off too. If it did not, the documented opt-out would be *stricter* than
    /// the default — a box would start unsandboxed but still be refused for
    /// lacking a cgroup.
    #[test]
    fn disabled_profile_allows_unlimited_host_resources() {
        let disabled = SecurityOptions::disabled();
        assert!(
            disabled.allow_unlimited_host_resources,
            "disabled() must not be stricter than default() on host resources"
        );
        assert!(
            !disabled.jailer_enabled,
            "sanity: disabled() really is the off profile"
        );
    }
}
