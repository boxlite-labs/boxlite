//! JailerBuilder for constructing a [`Jailer`](super::Jailer).

use super::Jailer;
use super::PathAccess;
use super::sandbox::{PlatformSandbox, Sandbox};
use crate::runtime::advanced_options::{ResourceLimits, SecurityOptions};
use crate::runtime::layout::BoxFilesystemLayout;
use crate::runtime::options::VolumeSpec;
use std::os::fd::RawFd;
use std::path::PathBuf;

/// Builder for constructing a [`Jailer`].
///
/// Uses a consuming builder pattern — each method takes ownership and returns
/// the modified builder, enabling fluent chains.
///
/// # Example
///
/// ```ignore
/// let jail = JailerBuilder::new()
///     .with_box_id("my-box")
///     .with_layout(layout)
///     .with_security(SecurityOptions::enabled())
///     .build()?;
/// ```
#[derive(Debug, Clone)]
pub struct JailerBuilder {
    security: SecurityOptions,
    volumes: Vec<VolumeSpec>,
    box_id: Option<String>,
    layout: Option<BoxFilesystemLayout>,
    preserved_fds: Vec<(RawFd, i32)>,
    detach: bool,
    additional_path_access: Vec<PathAccess>,
    network_backend_enabled: bool,
    /// VM guest memory in MiB, used to derive the host cgroup memory limit.
    vm_memory_mib: Option<u32>,
}

impl Default for JailerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl JailerBuilder {
    /// Create a new JailerBuilder with default settings.
    pub fn new() -> Self {
        Self {
            security: SecurityOptions::default(),
            volumes: Vec::new(),
            box_id: None,
            layout: None,
            preserved_fds: Vec::new(),
            detach: false,
            additional_path_access: Vec::new(),
            network_backend_enabled: false,
            vm_memory_mib: None,
        }
    }

    /// Set the box ID (required).
    pub fn with_box_id(mut self, id: impl Into<String>) -> Self {
        self.box_id = Some(id.into());
        self
    }

    /// Set the box filesystem layout (required).
    pub fn with_layout(mut self, layout: BoxFilesystemLayout) -> Self {
        self.layout = Some(layout);
        self
    }

    /// Set security options.
    pub fn with_security(mut self, security: SecurityOptions) -> Self {
        self.security = security;
        self
    }

    /// Set volume mounts.
    ///
    /// Volumes are used for sandbox path restrictions.
    /// All volumes are added to readable paths; writable volumes also get write access.
    pub fn with_volumes(mut self, volumes: Vec<VolumeSpec>) -> Self {
        self.volumes = volumes;
        self
    }

    /// Set the VM guest memory in MiB.
    ///
    /// Used to derive the host cgroup `memory.max` so the limit scales with
    /// the box's configured RAM instead of being a fixed value.
    pub fn with_vm_memory_mib(mut self, memory_mib: Option<u32>) -> Self {
        self.vm_memory_mib = memory_mib;
        self
    }

    /// Add a single volume mount.
    pub fn with_volume(mut self, volume: VolumeSpec) -> Self {
        self.volumes.push(volume);
        self
    }

    /// Enable or disable jailer isolation.
    pub fn with_jailer_enabled(mut self, enabled: bool) -> Self {
        self.security.jailer_enabled = enabled;
        self
    }

    /// Enable or disable seccomp filtering (Linux only).
    pub fn with_seccomp_enabled(mut self, enabled: bool) -> Self {
        self.security.seccomp_enabled = enabled;
        self
    }

    // ========================================================================
    // SECURITY FIELDS — fluent passthroughs for the rest of `SecurityOptions`.
    //
    // Until now `JailerBuilder` only exposed `jailer_enabled` /
    // `seccomp_enabled` even though `SecurityOptions` carries another
    // dozen knobs (UID/GID drop, namespaces, chroot, FD cleanup, env
    // allowlist, rlimits, macOS sandbox). Callers wanting any of those
    // had to construct a `SecurityOptions` separately and pipe it
    // through `with_security(...)`, which scattered build sites and
    // skipped past any future invariants the builder might enforce.
    //
    // Each setter below maps 1-to-1 to the matching `SecurityOptions`
    // field (same name minus the `with_` prefix). Naming and signature
    // mirror `SecurityOptionsBuilder` (`runtime/advanced_options.rs`)
    // except for `&mut self` → `mut self` to match `JailerBuilder`'s
    // consuming chain style. No defaults are introduced here — the
    // initial value still comes from `SecurityOptions::default()`.
    // ========================================================================

    /// Set the UID to drop to after setup (Linux only).
    ///
    /// - `Some(0)` keeps root (not recommended).
    /// - `Some(uid)` drops to that UID.
    /// - `None` (the default) leaves the auto-allocate behaviour in place.
    pub fn with_uid(mut self, uid: Option<u32>) -> Self {
        self.security.uid = uid;
        self
    }

    /// Set the GID to drop to after setup (Linux only). Same semantics
    /// as [`with_uid`](Self::with_uid).
    pub fn with_gid(mut self, gid: Option<u32>) -> Self {
        self.security.gid = gid;
        self
    }

    /// Enable / disable a new PID namespace (Linux only). When true,
    /// the shim becomes PID 1 inside the namespace.
    pub fn with_new_pid_ns(mut self, enabled: bool) -> Self {
        self.security.new_pid_ns = enabled;
        self
    }

    /// Enable / disable a new network namespace (Linux only). gvproxy
    /// already handles networking, so this is normally off — flip it
    /// on for fully isolated traffic.
    pub fn with_new_net_ns(mut self, enabled: bool) -> Self {
        self.security.new_net_ns = enabled;
        self
    }

    /// Set the base directory for chroot jails (Linux only). Default
    /// `/srv/boxlite`.
    pub fn with_chroot_base(mut self, path: impl Into<PathBuf>) -> Self {
        self.security.chroot_base = path.into();
        self
    }

    /// Enable / disable chroot via `pivot_root` (Linux only). Default
    /// `true` on Linux.
    pub fn with_chroot_enabled(mut self, enabled: bool) -> Self {
        self.security.chroot_enabled = enabled;
        self
    }

    /// Close inherited file descriptors (keeps stdin/stdout/stderr).
    /// Default `true`.
    pub fn with_close_fds(mut self, enabled: bool) -> Self {
        self.security.close_fds = enabled;
        self
    }

    /// Sanitize environment variables (keeps only `env_allowlist`).
    /// Default `true`.
    pub fn with_sanitize_env(mut self, enabled: bool) -> Self {
        self.security.sanitize_env = enabled;
        self
    }

    /// Replace the env-var allowlist wholesale. To add a single entry
    /// without throwing away the default, use
    /// [`with_allowed_env`](Self::with_allowed_env).
    pub fn with_env_allowlist(mut self, vars: Vec<String>) -> Self {
        self.security.env_allowlist = vars;
        self
    }

    /// Append a single name to the env-var allowlist (idempotent — a
    /// duplicate is ignored). The default list keeps `RUST_LOG`, `PATH`,
    /// `HOME`, `USER`, `LANG`.
    pub fn with_allowed_env(mut self, var: impl Into<String>) -> Self {
        let var = var.into();
        if !self.security.env_allowlist.contains(&var) {
            self.security.env_allowlist.push(var);
        }
        self
    }

    /// Replace all resource limits at once.
    pub fn with_resource_limits(mut self, limits: ResourceLimits) -> Self {
        self.security.resource_limits = limits;
        self
    }

    /// `RLIMIT_NOFILE` — maximum open file descriptors.
    pub fn with_max_open_files(mut self, limit: u64) -> Self {
        self.security.resource_limits.max_open_files = Some(limit);
        self
    }

    /// `RLIMIT_FSIZE` — maximum file size, in bytes.
    pub fn with_max_file_size_bytes(mut self, bytes: u64) -> Self {
        self.security.resource_limits.max_file_size = Some(bytes);
        self
    }

    /// `RLIMIT_NPROC` — maximum number of processes.
    pub fn with_max_processes(mut self, limit: u64) -> Self {
        self.security.resource_limits.max_processes = Some(limit);
        self
    }

    /// `RLIMIT_AS` — maximum virtual memory, in bytes.
    pub fn with_max_memory_bytes(mut self, bytes: u64) -> Self {
        self.security.resource_limits.max_memory = Some(bytes);
        self
    }

    /// `RLIMIT_CPU` — maximum CPU time, in seconds.
    pub fn with_max_cpu_time_seconds(mut self, seconds: u64) -> Self {
        self.security.resource_limits.max_cpu_time = Some(seconds);
        self
    }

    /// Custom sandbox-exec profile path (macOS only). When `None` the
    /// built-in modular profile is used.
    pub fn with_sandbox_profile(mut self, path: Option<PathBuf>) -> Self {
        self.security.sandbox_profile = path;
        self
    }

    /// Allow / deny network access inside the sandbox profile (Linux landlock
    /// + macOS seatbelt). Default `true` because gvproxy networking needs it.
    pub fn with_network_enabled(mut self, enabled: bool) -> Self {
        self.security.network_enabled = enabled;
        self
    }

    // -------- preset shortcuts -----------------------------------------------
    //
    // Mirror `SecurityOptions::{enabled, disabled}` so a caller can pick a
    // baseline with the same fluent style they use for the rest of the builder.

    /// Replace the security profile with the fully-**enabled** profile (the
    /// default) — every supported isolation feature on.
    pub fn with_security_enabled(mut self) -> Self {
        self.security = SecurityOptions::enabled();
        self
    }

    /// Replace the security profile with the **disabled** profile — master
    /// switch off, every sub-protection off. For debugging / unsandboxable envs.
    pub fn with_security_disabled(mut self) -> Self {
        self.security = SecurityOptions::disabled();
        self
    }

    /// Preserve an FD through pre_exec by dup2'ing source to target.
    ///
    /// The pre_exec hook dup2s source to target before FD cleanup runs.
    /// All FDs above the highest target are closed; target FDs are kept.
    /// Used for watchdog pipe inheritance across fork.
    pub fn with_preserved_fd(mut self, source: RawFd, target: i32) -> Self {
        self.preserved_fds.push((source, target));
        self
    }

    /// Configure detach-mode process isolation applied to the spawned
    /// child: `detach=true` → `setsid()` in `pre_exec` (daemon, own
    /// session); `detach=false` → `cmd.process_group(0)` (own pgroup
    /// for atomic `killpg` cleanup).
    pub fn with_detach(mut self, detach: bool) -> Self {
        self.detach = detach;
        self
    }

    /// Add caller-defined filesystem permissions required by the confined shim.
    pub(crate) fn with_additional_path_access(mut self, paths: Vec<PathAccess>) -> Self {
        self.additional_path_access = paths;
        self
    }

    /// Grant the confined shim the AF_UNIX endpoints used by its network backend.
    pub(crate) fn with_network_backend_enabled(mut self, enabled: bool) -> Self {
        self.network_backend_enabled = enabled;
        self
    }

    /// Build with the platform-default sandbox.
    ///
    /// On Linux: [`BwrapSandbox`](super::sandbox::BwrapSandbox)
    /// On macOS: [`SeatbeltSandbox`](super::sandbox::SeatbeltSandbox)
    /// On other: [`NoopSandbox`](super::sandbox::NoopSandbox)
    ///
    /// # Errors
    ///
    /// Returns [`JailerError::Config`](super::JailerError) with
    /// [`ConfigError::InvalidConfig`](super::ConfigError) if `box_id` or `box_dir` was not set.
    pub fn build(self) -> Result<Jailer<PlatformSandbox>, crate::jailer::JailerError> {
        self.build_with(PlatformSandbox::platform_new())
    }

    /// Build with a custom sandbox implementation.
    ///
    /// Useful for testing or injecting alternative sandbox behavior.
    ///
    /// # Errors
    ///
    /// Returns [`JailerError::Config`](super::JailerError) with
    /// [`ConfigError::InvalidConfig`](super::ConfigError) if `box_id` or `layout` was not set.
    pub fn build_with<S: Sandbox>(
        self,
        sandbox: S,
    ) -> Result<Jailer<S>, crate::jailer::JailerError> {
        let box_id = self.box_id.ok_or_else(|| {
            crate::jailer::ConfigError::InvalidConfig("box_id is required".to_string())
        })?;

        let layout = self.layout.ok_or_else(|| {
            crate::jailer::ConfigError::InvalidConfig("layout is required".to_string())
        })?;

        // Surface fields that this platform silently ignores (flat struct mixes
        // Linux-only and macOS-only knobs).
        self.security.warn_inert_fields();

        Ok(Jailer {
            sandbox,
            security: self.security,
            volumes: self.volumes,
            box_id,
            layout,
            preserved_fds: self.preserved_fds,
            detach: self.detach,
            additional_path_access: self.additional_path_access,
            network_backend_enabled: self.network_backend_enabled,
            vm_memory_mib: self.vm_memory_mib,
        })
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::layout::FsLayoutConfig;
    use std::path::{Path, PathBuf};

    /// Create a test layout from a box directory path.
    fn test_layout(box_dir: impl Into<PathBuf>) -> BoxFilesystemLayout {
        BoxFilesystemLayout::new(box_dir.into(), FsLayoutConfig::without_bind_mount(), false)
    }

    #[test]
    fn test_builder_basic() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .build()
            .expect("Should build successfully");

        assert_eq!(jailer.box_id(), "test-box");
        assert_eq!(jailer.box_dir(), Path::new("/tmp/box"));
    }

    #[test]
    fn test_builder_missing_box_id() {
        let result = JailerBuilder::new()
            .with_layout(test_layout("/tmp/box"))
            .build();

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("box_id"));
    }

    #[test]
    fn test_builder_missing_layout() {
        let result = JailerBuilder::new().with_box_id("test-box").build();

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("layout"));
    }

    #[test]
    fn test_builder_with_security() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_security(SecurityOptions::enabled())
            .build()
            .expect("Should build successfully");

        assert!(jailer.security().jailer_enabled);
    }

    #[test]
    fn test_builder_consuming_chain() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_jailer_enabled(true)
            .build()
            .expect("Should build successfully");

        assert!(jailer.security().jailer_enabled);
    }

    #[test]
    fn test_builder_with_volume() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_volume(VolumeSpec {
                managed_volume: None,
                host_path: "/data".to_string(),
                guest_path: "/mnt/data".to_string(),
                read_only: true,
            })
            .with_volume(VolumeSpec {
                managed_volume: None,
                host_path: "/output".to_string(),
                guest_path: "/mnt/output".to_string(),
                read_only: false,
            })
            .build()
            .expect("Should build successfully");

        assert_eq!(jailer.volumes().len(), 2);
    }

    #[test]
    fn builder_propagates_additional_path_access() {
        let extra = PathBuf::from("/host/required-by-vmm");
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_additional_path_access(vec![PathAccess {
                path: extra.clone(),
                writable: false,
            }])
            .build()
            .expect("Should build successfully");

        let context = jailer.context();
        let propagated = context
            .paths
            .iter()
            .find(|entry| entry.path == extra)
            .expect("additional path access should reach the sandbox context");
        assert!(!propagated.writable);
    }

    #[test]
    fn test_builder_with_custom_sandbox() {
        use crate::jailer::NoopSandbox;

        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .build_with(NoopSandbox::new())
            .expect("Should build with custom sandbox");

        assert_eq!(jailer.box_id(), "test-box");
    }

    // ===========================================================
    // SecurityOptions fluent passthroughs
    //
    // One test per new public method, all on the same fluent chain
    // so a regression that drops any setter shows up as a single
    // failing assertion rather than a compile error elsewhere.
    // Reverting (deleting) any `with_*` setter on the builder breaks
    // the corresponding line below.
    // ===========================================================

    #[test]
    fn builder_exposes_uid_gid_passthroughs() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_uid(Some(1234))
            .with_gid(Some(5678))
            .build()
            .expect("should build");
        assert_eq!(jailer.security().uid, Some(1234));
        assert_eq!(jailer.security().gid, Some(5678));
    }

    #[test]
    fn builder_exposes_namespace_toggles() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_new_pid_ns(true)
            .with_new_net_ns(true)
            .build()
            .expect("should build");
        assert!(jailer.security().new_pid_ns);
        assert!(jailer.security().new_net_ns);
    }

    #[test]
    fn builder_exposes_chroot_settings() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_chroot_base("/var/empty")
            .with_chroot_enabled(false)
            .build()
            .expect("should build");
        assert_eq!(
            jailer.security().chroot_base,
            std::path::PathBuf::from("/var/empty")
        );
        assert!(!jailer.security().chroot_enabled);
    }

    #[test]
    fn builder_exposes_env_and_fd_hygiene() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_close_fds(false)
            .with_sanitize_env(false)
            .with_env_allowlist(vec!["FOO".into(), "BAR".into()])
            .with_allowed_env("BAZ")
            .with_allowed_env("FOO") // dup must be ignored
            .build()
            .expect("should build");
        assert!(!jailer.security().close_fds);
        assert!(!jailer.security().sanitize_env);
        assert_eq!(
            jailer.security().env_allowlist,
            vec!["FOO".to_string(), "BAR".to_string(), "BAZ".to_string()]
        );
    }

    #[test]
    fn builder_exposes_resource_limits() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_max_open_files(2048)
            .with_max_file_size_bytes(1_000_000)
            .with_max_processes(64)
            .with_max_memory_bytes(2 * 1024 * 1024 * 1024)
            .with_max_cpu_time_seconds(900)
            .build()
            .expect("should build");
        let rl = &jailer.security().resource_limits;
        assert_eq!(rl.max_open_files, Some(2048));
        assert_eq!(rl.max_file_size, Some(1_000_000));
        assert_eq!(rl.max_processes, Some(64));
        assert_eq!(rl.max_memory, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(rl.max_cpu_time, Some(900));
    }

    #[test]
    fn builder_exposes_resource_limits_bulk_set() {
        let bulk = ResourceLimits {
            max_open_files: Some(99),
            ..Default::default()
        };
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_resource_limits(bulk)
            .build()
            .expect("should build");
        assert_eq!(jailer.security().resource_limits.max_open_files, Some(99));
    }

    #[test]
    fn builder_exposes_macos_sandbox_knobs() {
        let jailer = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_sandbox_profile(Some("/etc/box.sb".into()))
            .with_network_enabled(false)
            .build()
            .expect("should build");
        assert_eq!(
            jailer.security().sandbox_profile,
            Some(std::path::PathBuf::from("/etc/box.sb"))
        );
        assert!(!jailer.security().network_enabled);
    }

    #[test]
    fn builder_preset_shortcuts_pick_known_profiles() {
        // Development → relaxed: jailer/chroot/close_fds off.
        let dev = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_security_disabled()
            .build()
            .expect("should build dev");
        assert!(!dev.security().jailer_enabled);

        // Maximum → strict: jailer on; seccomp / namespaces on where the
        // platform supports them (Linux).
        let max = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_security_enabled()
            .build()
            .expect("should build max");
        assert!(max.security().jailer_enabled);
        // seccomp is Linux-only (SecurityOptions::default gates it on cfg), so the
        // maximum profile enables it on Linux and correctly leaves it off elsewhere.
        assert_eq!(max.security().seccomp_enabled, cfg!(target_os = "linux"));

        // Standard is the recommended default; just confirm it
        // overrides whatever the chain set before.
        let std_p = JailerBuilder::new()
            .with_box_id("test-box")
            .with_layout(test_layout("/tmp/box"))
            .with_uid(Some(0)) // would normally be clobbered
            .with_security_enabled()
            .build()
            .expect("should build std");
        assert!(std_p.security().jailer_enabled);
        // Preset wholesale-replaces, so the uid=0 set above is gone.
        assert_ne!(std_p.security().uid, Some(0));
    }
}
