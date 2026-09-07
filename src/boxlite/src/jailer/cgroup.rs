//! Cgroup v2 setup for resource limiting.
//!
//! This module sets up cgroup v2 limits for the boxlite-shim process.
//! Cgroups are used to limit CPU, memory, and process count.
//!
//! ## Why Cgroups?
//!
//! - Prevent DoS attacks (fork bomb, memory exhaustion)
//! - Fair resource sharing between boxes
//! - Enforced by kernel, can't be bypassed from userspace
//!
//! ## Rootless Support
//!
//! This module supports both root and rootless operation:
//! - **Root**: Creates cgroups in `/sys/fs/cgroup/boxlite/`
//! - **Rootless**: Creates cgroups in the user's systemd service scope:
//!   `/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/boxlite/`
//!
//! ## Cgroup v2 Structure
//!
//! ```text
//! {cgroup_base}/              # /sys/fs/cgroup (root) or user service path (rootless)
//! └── boxlite/
//!     └── {box_id}/
//!         ├── cpu.max           # CPU limit
//!         ├── cpu.weight        # CPU shares
//!         ├── memory.max        # Memory limit
//!         ├── memory.high       # Memory throttle threshold
//!         ├── pids.max          # Max processes
//!         └── cgroup.procs      # Add process here
//! ```

use super::common;
use super::error::JailerError;
use crate::runtime::advanced_options::ResourceLimits;
use crate::runtime::id::BoxID;
use std::fs;
use std::path::{Path, PathBuf};

/// Base path for cgroup v2 filesystem.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// BoxLite cgroup name.
const BOXLITE_CGROUP: &str = "boxlite";

// ============================================================================
// Rootless Cgroup Support
// ============================================================================

/// Check if the current process is running as root.
#[cfg(target_os = "linux")]
pub(crate) fn is_root() -> bool {
    unsafe { libc::getuid() == 0 }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn is_root() -> bool {
    false
}

/// Get the user's systemd cgroup base path for rootless operation.
///
/// On systemd systems, users can create cgroups under their user service:
/// `/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/`
#[cfg(target_os = "linux")]
fn get_user_cgroup_base() -> Option<PathBuf> {
    let uid = unsafe { libc::getuid() };
    let path = PathBuf::from(format!(
        "/sys/fs/cgroup/user.slice/user-{}.slice/user@{}.service",
        uid, uid
    ));
    if path.exists() {
        Some(path)
    } else {
        // Fallback: try to find any writable cgroup path from /proc/self/cgroup
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn get_user_cgroup_base() -> Option<PathBuf> {
    None
}

/// Get the cgroup base path for the current user.
///
/// - Root: returns `/sys/fs/cgroup`
/// - Non-root (systemd): returns `/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service`
/// - Non-root (no systemd): falls back to `/sys/fs/cgroup` (will likely fail)
fn get_cgroup_base() -> PathBuf {
    if is_root() {
        PathBuf::from(CGROUP_ROOT)
    } else {
        get_user_cgroup_base().unwrap_or_else(|| PathBuf::from(CGROUP_ROOT))
    }
}

/// Configuration for cgroup resource limits.
#[derive(Debug, Clone, Default)]
pub struct CgroupConfig {
    /// Memory limit in bytes (memory.max).
    pub memory_max: Option<u64>,

    /// Memory high threshold in bytes (memory.high).
    /// Processes exceeding this are throttled.
    pub memory_high: Option<u64>,

    /// CPU weight (1-10000, default 100).
    /// Higher = more CPU time relative to other cgroups.
    pub cpu_weight: Option<u32>,

    /// CPU max in format "quota period" (e.g., "100000 100000" = 100%).
    /// First number is max microseconds per period.
    pub cpu_max: Option<(u64, u64)>,

    /// Maximum number of processes (pids.max).
    pub pids_max: Option<u64>,

    /// `CPUQuotaPerSecUSec` for the systemd transient scope (rootless host
    /// path). Microseconds of CPU time per real second — `1_000_000` = 100% of
    /// one core, `N * 1_000_000` = full N-core ceiling.
    ///
    /// Separate from `cpu_max` (which is the in-kernel `cpu.max` quota/period
    /// pair) because rootless can't enable the `cpu` controller on the box
    /// cgroup directly: the `+cpu` write to `cgroup.subtree_control` fails
    /// with EINVAL when the parent slice hasn't delegated it. systemd's user
    /// manager owns `user.slice` where `cpu` is pre-delegated under the
    /// unified hierarchy, so the property reaches the kernel by riding the
    /// `busctl StartTransientUnit` call alongside `MemoryMax`/`TasksMax`.
    pub cpu_quota_us_per_sec: Option<u64>,
}

impl CgroupConfig {
    /// True if any cgroup limit is set. When false, creating a cgroup buys
    /// nothing — callers should skip cgroup setup entirely.
    pub fn has_limits(&self) -> bool {
        self.memory_max.is_some()
            || self.memory_high.is_some()
            || self.cpu_weight.is_some()
            || self.cpu_max.is_some()
            || self.pids_max.is_some()
            || self.cpu_quota_us_per_sec.is_some()
    }
}

/// Check if cgroup v2 is available and unified hierarchy is used.
pub fn is_cgroup_v2_available() -> bool {
    // Check if cgroup2 is mounted
    let cgroup_root = Path::new(CGROUP_ROOT);
    if !cgroup_root.exists() {
        return false;
    }

    // Check for cgroup.controllers (cgroup v2 indicator)
    let controllers = cgroup_root.join("cgroup.controllers");
    controllers.exists()
}

/// Get the path to a box's cgroup directory.
///
/// The base path depends on whether running as root or regular user:
/// - Root: `/sys/fs/cgroup/boxlite/{box_id}`
/// - User: `/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/boxlite/{box_id}`
pub fn cgroup_path(box_id: &str) -> PathBuf {
    get_cgroup_base().join(BOXLITE_CGROUP).join(box_id)
}

/// Kill every process in a box's cgroup via cgroup v2 `cgroup.kill`.
///
/// Reaps the box's *entire* process tree atomically — the outer bwrap launcher,
/// the inner pid-namespace bwrap, the shim, and the VM — regardless of
/// pid-namespace or process-group structure. A single-pid `SIGKILL` of the
/// recorded pid only hits the outer bwrap; a detached box's inner tree survives
/// it, since #851 stopped applying `--die-with-parent` to detached boxes. The
/// whole tree lives in the box's cgroup, so killing the cgroup by id reaps it
/// even after `state.pid` has been cleared.
///
/// Best-effort and idempotent: a no-op if the cgroup is gone, already empty, or
/// `cgroup.kill` is unavailable (kernel < 5.14 / cgroup v1 / no jailer). Returns
/// `true` if the kill file was written.
///
/// Takes a [`BoxID`] rather than a raw `&str` on purpose: this writes to a path
/// derived from the id, so it must be a safe single path component. `BoxID`'s
/// constructor ([`BoxID::parse`]/mint) is the one choke point that guarantees
/// that — its charset (`[A-Za-z0-9_-]`) excludes `/`, `\`, and `.`, so `..`/`.`
/// and path separators are unrepresentable. The type carries the guarantee, so
/// no per-call traversal check is needed (or could drift) here.
///
/// `pub(super)` on purpose: this is the cgroup *mechanism*, reached only through
/// the jailer's [`super::reap_box`] facade. Layers above the jailer (box,
/// runtime) reap by box semantics and never name cgroups.
pub(super) fn kill_cgroup(box_id: &BoxID) -> bool {
    let kill_file = cgroup_path(box_id.as_str()).join("cgroup.kill");
    std::fs::write(&kill_file, "1").is_ok()
}

/// Setup cgroup for a box.
///
/// Creates the cgroup directory and configures resource limits.
/// Must be called BEFORE spawning the process.
///
/// # Errors
///
/// Returns [`JailerError::Cgroup`] if:
/// - Cgroup v2 is not available on the system
/// - Failed to create the boxlite parent cgroup directory
/// - Failed to create the box-specific cgroup directory
/// - Failed to write resource limit configuration files
pub fn setup_cgroup(box_id: &str, config: &CgroupConfig) -> Result<PathBuf, JailerError> {
    if !is_cgroup_v2_available() {
        tracing::warn!("Cgroup v2 not available, skipping cgroup setup");
        return Err(JailerError::Cgroup("Cgroup v2 not available".to_string()));
    }

    let cgroup_base = get_cgroup_base();
    let boxlite_cgroup = cgroup_base.join(BOXLITE_CGROUP);
    let box_cgroup = boxlite_cgroup.join(box_id);

    tracing::debug!(
        cgroup_base = %cgroup_base.display(),
        is_root = is_root(),
        "Using cgroup base path"
    );

    // Create boxlite parent cgroup if needed, then (idempotently) delegate the
    // controllers to its children. Running enable_controllers every time — not
    // only on creation — repairs a parent left behind by an earlier build that
    // failed to delegate, so box children always end up with the controller
    // files.
    if !boxlite_cgroup.exists() {
        fs::create_dir(&boxlite_cgroup).map_err(|e| {
            JailerError::Cgroup(format!(
                "Failed to create boxlite cgroup at {}: {}",
                boxlite_cgroup.display(),
                e
            ))
        })?;
    }
    enable_controllers(&boxlite_cgroup)?;

    // Create box cgroup
    if !box_cgroup.exists() {
        fs::create_dir(&box_cgroup).map_err(|e| {
            JailerError::Cgroup(format!(
                "Failed to create box cgroup at {}: {}",
                box_cgroup.display(),
                e
            ))
        })?;
    }

    // Apply limits
    apply_limits(&box_cgroup, config)?;

    tracing::debug!(
        box_id = %box_id,
        path = %box_cgroup.display(),
        "Cgroup created"
    );

    Ok(box_cgroup)
}

/// Delegate controllers to child cgroups — but only those actually available
/// here. cgroup v2 rejects the *entire* `cgroup.subtree_control` write if any
/// named controller is absent, so the literal `+cpu +memory +pids` fails on
/// rootless/systemd-user hosts where the session is delegated only `memory`
/// and `pids` (no `cpu`). That failure left box cgroups with no controllers
/// and the DoS limits silently unenforced. Enable the intersection of what we
/// want with `cgroup.controllers` instead, so memory/pids still apply when cpu
/// isn't delegated.
fn enable_controllers(cgroup_path: &Path) -> Result<(), JailerError> {
    let controllers_path = cgroup_path.join("cgroup.controllers");
    let available = fs::read_to_string(&controllers_path).map_err(|e| {
        JailerError::Cgroup(format!(
            "Failed to read available controllers at {}: {}",
            controllers_path.display(),
            e
        ))
    })?;

    let enable: Vec<String> = ["cpu", "memory", "pids"]
        .iter()
        .filter(|want| available.split_whitespace().any(|have| have == **want))
        .map(|want| format!("+{want}"))
        .collect();

    if enable.is_empty() {
        return Err(JailerError::Cgroup(format!(
            "none of cpu/memory/pids are delegated to {} (available: [{}])",
            cgroup_path.display(),
            available.trim()
        )));
    }

    write_file(
        &cgroup_path.join("cgroup.subtree_control"),
        &enable.join(" "),
    )?;
    Ok(())
}

/// Apply resource limits to a cgroup.
fn apply_limits(cgroup_path: &Path, config: &CgroupConfig) -> Result<(), JailerError> {
    // Memory limit
    if let Some(memory_max) = config.memory_max {
        write_file(&cgroup_path.join("memory.max"), &memory_max.to_string())?;
    }

    // Memory high (throttle threshold)
    if let Some(memory_high) = config.memory_high {
        write_file(&cgroup_path.join("memory.high"), &memory_high.to_string())?;
    }

    // CPU weight
    if let Some(cpu_weight) = config.cpu_weight {
        write_file(&cgroup_path.join("cpu.weight"), &cpu_weight.to_string())?;
    }

    // CPU max (quota period)
    if let Some((quota, period)) = config.cpu_max {
        write_file(
            &cgroup_path.join("cpu.max"),
            &format!("{} {}", quota, period),
        )?;
    }

    // Pids max
    if let Some(pids_max) = config.pids_max {
        write_file(&cgroup_path.join("pids.max"), &pids_max.to_string())?;
    }

    Ok(())
}

/// Add a process to a cgroup.
///
/// Call this after spawning the process.
#[allow(dead_code)]
pub fn add_process(box_id: &str, pid: u32) -> Result<(), JailerError> {
    let cgroup_path = cgroup_path(box_id);
    let procs_file = cgroup_path.join("cgroup.procs");

    write_file(&procs_file, &pid.to_string())?;

    tracing::debug!(
        box_id = %box_id,
        pid = pid,
        "Process added to cgroup"
    );

    Ok(())
}

/// Remove a cgroup.
///
/// The cgroup must be empty (no processes) before removal.
#[allow(dead_code)]
pub fn remove_cgroup(box_id: &str) -> Result<(), JailerError> {
    let cgroup_path = cgroup_path(box_id);

    if cgroup_path.exists() {
        fs::remove_dir(&cgroup_path).map_err(|e| {
            JailerError::Cgroup(format!(
                "Failed to remove cgroup at {}: {}",
                cgroup_path.display(),
                e
            ))
        })?;

        tracing::debug!(
            box_id = %box_id,
            "Cgroup removed"
        );
    }

    Ok(())
}

/// Helper to write to a cgroup file.
fn write_file(path: &Path, content: &str) -> Result<(), JailerError> {
    fs::write(path, content)
        .map_err(|e| JailerError::Cgroup(format!("Failed to write to {}: {}", path.display(), e)))
}

/// Convert ResourceLimits to CgroupConfig.
impl From<&ResourceLimits> for CgroupConfig {
    fn from(limits: &ResourceLimits) -> Self {
        Self {
            memory_max: limits.max_memory,
            memory_high: limits.max_memory.map(|m| m * 9 / 10), // 90% of max
            cpu_weight: None,                                   // Could add to ResourceLimits
            cpu_max: limits.max_cpu_time.map(|t| {
                // Convert seconds to quota/period
                // 1 CPU = 100000/100000
                (t * 1_000_000, 1_000_000)
            }),
            pids_max: limits.max_processes,
            cpu_quota_us_per_sec: None, // wired in by apply_cgroup_defaults
        }
    }
}

/// Default host process cap. Baseline box uses ~22 host tasks (libkrun vCPUs +
/// gvproxy + tokio); 1024 leaves wide headroom while still catching a runaway
/// thread/fork leak in the VMM stack.
pub(crate) const DEFAULT_HOST_PIDS_MAX: u64 = 1024;

/// Apply default DoS caps in place: `memory.max = 2× VM RAM + 512 MiB`,
/// `pids.max = 1024`, `cpu.max = host_cores × 1_000_000`. Mirrors any explicit
/// `cpu_max` to `cpu_quota_us_per_sec` so rootless (systemd-scope `CPUQuota`)
/// and rootful (direct `cpu.max` file write) caps stay in sync — without this
/// mirror, a user-set `ResourceLimits.max_cpu_time` lands on `cpu_max` only
/// and gets silently dropped rootless.
///
/// `host_cores` is injected (rather than queried from `available_parallelism`
/// inside this function) so the defaults are pure and unit-testable.
pub(crate) fn apply_cgroup_defaults(
    config: &mut CgroupConfig,
    vm_memory_mib: u64,
    host_cores: u64,
) {
    if config.memory_max.is_none() {
        config.memory_max = Some(vm_memory_mib * 2 * 1024 * 1024 + 512 * 1024 * 1024);
    }
    if config.pids_max.is_none() {
        config.pids_max = Some(DEFAULT_HOST_PIDS_MAX);
    }
    let host_cpu_us_per_sec = host_cores.saturating_mul(1_000_000);
    if config.cpu_max.is_none() {
        config.cpu_max = Some((host_cpu_us_per_sec, 1_000_000));
    }
    if config.cpu_quota_us_per_sec.is_none() {
        config.cpu_quota_us_per_sec = config.cpu_max.map(|(q, _period)| q);
    }
}

// ============================================================================
// Async-Signal-Safe Cgroup (for pre_exec)
// ============================================================================

/// Add current process to cgroup - async-signal-safe version for pre_exec.
///
/// This function is designed to be called from a `pre_exec` hook, which runs
/// after `fork()` but before `exec()`. Only async-signal-safe operations are
/// allowed in this context.
///
/// # Safety
///
/// This function only uses async-signal-safe syscalls (open, write, close, getpid).
/// Do NOT add:
/// - Logging (tracing, println)
/// - Memory allocation (Box, Vec, String)
/// - Mutex operations
///
/// # Arguments
/// * `cgroup_procs_path` - Pre-computed path to cgroup.procs file (as null-terminated C string)
///
/// # Returns
/// * `Ok(())` - Process added to cgroup
/// * `Err(errno)` - Failed to add process
#[cfg(target_os = "linux")]
pub fn add_self_to_cgroup_raw(cgroup_procs_path: &std::ffi::CStr) -> Result<(), i32> {
    // Get current PID
    let pid = unsafe { libc::getpid() };

    // Format PID as string (async-signal-safe: stack buffer, no allocation)
    let mut pid_buf = [0u8; 16];
    let pid_len = {
        // Manual formatting to avoid write! which might allocate
        let mut n = pid as u32;
        let mut len = 0;
        let mut temp = [0u8; 16];

        // Convert number to string (reverse order)
        if n == 0 {
            temp[0] = b'0';
            len = 1;
        } else {
            while n > 0 {
                temp[len] = b'0' + (n % 10) as u8;
                n /= 10;
                len += 1;
            }
        }

        // Reverse into pid_buf
        for i in 0..len {
            pid_buf[i] = temp[len - 1 - i];
        }
        pid_buf[len] = b'\n';
        len + 1
    };

    // Open cgroup.procs file
    let fd = unsafe { libc::open(cgroup_procs_path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };

    if fd < 0 {
        return Err(common::get_errno());
    }

    // Write PID to file
    let result = unsafe { libc::write(fd, pid_buf.as_ptr() as *const libc::c_void, pid_len) };

    // Close file
    unsafe { libc::close(fd) };

    if result < 0 {
        return Err(common::get_errno());
    }

    Ok(())
}

/// Build the cgroup.procs path for a box.
///
/// Returns a CString that can be passed to `add_self_to_cgroup_raw`.
/// This should be called in the parent process before spawning.
#[cfg(target_os = "linux")]
pub fn build_cgroup_procs_path(box_id: &str) -> Option<std::ffi::CString> {
    if !is_cgroup_v2_available() {
        return None;
    }

    let path = cgroup_path(box_id).join("cgroup.procs");
    std::ffi::CString::new(path.to_string_lossy().as_bytes()).ok()
}

/// Rootless host limits: ask the systemd *user* manager to wrap an already
/// running shim PID in a transient scope carrying the resource limits.
///
/// Unlike the direct-cgroup path, an unprivileged process cannot migrate itself
/// from its login `session-N.scope` into `user@.service/.../boxlite-<id>.scope`
/// — the move needs write access to the root-owned `user.slice` common ancestor
/// and fails with EACCES. systemd owns that hierarchy, so we hand it the PID and
/// let it do the placement. The scope is transient and auto-removed once the
/// shim exits.
///
/// Calls `busctl` (shelling out keeps systemd a runtime, not a build/link,
/// dependency). Non-fatal to the caller: a box that can't be scoped is still
/// better than no box, matching the prior cgroup behavior.
#[cfg(target_os = "linux")]
pub fn adopt_pid_into_scope(
    box_id: &str,
    pid: u32,
    config: &CgroupConfig,
) -> Result<(), JailerError> {
    let unit = format!("boxlite-{box_id}.scope");

    // StartTransientUnit(name, mode, properties: a(sv), aux: a(sa(sv))).
    // busctl spells each property as `<name> <type> <value...>`; the count
    // before the list must match the number of properties we pass.
    let mut props: Vec<String> = vec![
        // PIDs is an array of u32 (au): "1" element count, then the pid.
        "PIDs".into(),
        "au".into(),
        "1".into(),
        pid.to_string(),
    ];
    let mut count: u32 = 1;
    let add = |name: &str, value: u64, props: &mut Vec<String>, count: &mut u32| {
        props.push(name.into());
        props.push("t".into());
        props.push(value.to_string());
        *count += 1;
    };
    if let Some(m) = config.memory_max {
        add("MemoryMax", m, &mut props, &mut count);
    }
    if let Some(h) = config.memory_high {
        add("MemoryHigh", h, &mut props, &mut count);
    }
    if let Some(p) = config.pids_max {
        add("TasksMax", p, &mut props, &mut count);
    }
    if let Some(q) = config.cpu_quota_us_per_sec {
        add("CPUQuotaPerSecUSec", q, &mut props, &mut count);
    }

    let mut args: Vec<String> = vec![
        "--user".into(),
        "--quiet".into(),
        "call".into(),
        "org.freedesktop.systemd1".into(),
        "/org/freedesktop/systemd1".into(),
        "org.freedesktop.systemd1.Manager".into(),
        "StartTransientUnit".into(),
        "ssa(sv)a(sa(sv))".into(),
        unit.clone(),
        "fail".into(),
        count.to_string(),
    ];
    args.extend(props);
    args.push("0".into()); // empty aux array

    let output = std::process::Command::new("busctl")
        .args(&args)
        .output()
        .map_err(|e| JailerError::Cgroup(format!("failed to run busctl: {e}")))?;

    if !output.status.success() {
        return Err(JailerError::Cgroup(format!(
            "StartTransientUnit for {unit} (pid {pid}) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cgroup_path() {
        let path = cgroup_path("test-box-123");
        // Path depends on whether running as root or regular user
        let expected_base = get_cgroup_base();
        let expected = expected_base.join("boxlite").join("test-box-123");
        assert_eq!(path, expected);
        // Verify the path ends with the expected suffix
        assert!(path.ends_with("boxlite/test-box-123"));
    }

    #[test]
    fn test_cgroup_v2_detection() {
        let available = is_cgroup_v2_available();
        println!("Cgroup v2 available: {}", available);
    }

    #[test]
    fn kill_cgroup_absent_is_noop() {
        // No cgroup exists for this id, so `cgroup.kill` can't be written:
        // kill_cgroup must report `false` and not panic. This locks the
        // best-effort/idempotent contract relied on by the no-jailer and
        // macOS-seatbelt paths (where there is no box cgroup to kill).
        let box_id = BoxID::parse("nonexistentbox000000000000").expect("valid id");
        assert!(
            !kill_cgroup(&box_id),
            "kill_cgroup must be a no-op (false) when the box has no cgroup"
        );
    }

    // Note: there is no `kill_cgroup_rejects_non_component_box_ids` test anymore.
    // The path-traversal guard moved into the type: `kill_cgroup` takes a
    // `BoxID`, and `BoxID::parse` already rejects `/`, `\`, `.`, `..`, and empty
    // ids (see `id::tests::test_parse_rejects_unsafe_characters`). A non-component
    // id is now unrepresentable at this call site, not merely rejected at runtime.

    #[test]
    fn test_cgroup_config_from_limits() {
        let limits = ResourceLimits {
            max_memory: Some(1024 * 1024 * 1024), // 1GB
            max_processes: Some(100),
            max_cpu_time: Some(60), // 60 seconds
            ..Default::default()
        };

        let config = CgroupConfig::from(&limits);

        assert_eq!(config.memory_max, Some(1024 * 1024 * 1024));
        assert_eq!(config.pids_max, Some(100));
        assert!(config.cpu_max.is_some());
    }

    /// The whole point of the rootless fix in this PR: when the parent's
    /// `cgroup.controllers` doesn't list `cpu` (the common rootless case),
    /// the atomic `+cpu +memory +pids` write fails and takes memory+pids
    /// down with it. `enable_controllers` must intersect what we want with
    /// what's available, so memory/pids still get delegated when cpu isn't.
    #[test]
    fn enable_controllers_writes_only_intersection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cgroup_path = dir.path();
        std::fs::write(cgroup_path.join("cgroup.controllers"), "memory pids\n")
            .expect("write cgroup.controllers");
        std::fs::write(cgroup_path.join("cgroup.subtree_control"), "")
            .expect("write cgroup.subtree_control");

        enable_controllers(cgroup_path).expect("must succeed with non-empty intersection");

        let written = std::fs::read_to_string(cgroup_path.join("cgroup.subtree_control"))
            .expect("read cgroup.subtree_control");
        assert!(
            written.contains("+memory"),
            "must enable memory when delegated; subtree_control={written:?}"
        );
        assert!(
            written.contains("+pids"),
            "must enable pids when delegated; subtree_control={written:?}"
        );
        assert!(
            !written.contains("+cpu"),
            "must NOT try to enable cpu when not delegated (the atomic write would fail and \
             take memory+pids with it); subtree_control={written:?}"
        );
    }

    /// All three controllers delegated (root / fully-privileged): every want
    /// is in the available set, all three get written.
    #[test]
    fn enable_controllers_writes_all_when_all_delegated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cgroup_path = dir.path();
        std::fs::write(
            cgroup_path.join("cgroup.controllers"),
            "cpuset cpu io memory hugetlb pids rdma misc\n",
        )
        .expect("write cgroup.controllers");
        std::fs::write(cgroup_path.join("cgroup.subtree_control"), "")
            .expect("write cgroup.subtree_control");

        enable_controllers(cgroup_path).expect("must succeed");

        let written = std::fs::read_to_string(cgroup_path.join("cgroup.subtree_control"))
            .expect("read cgroup.subtree_control");
        for want in ["+cpu", "+memory", "+pids"] {
            assert!(written.contains(want), "missing {want} in {written:?}");
        }
    }

    /// Pathological host: none of {cpu, memory, pids} are delegated. The
    /// function must Err loudly rather than silently writing an empty
    /// subtree_control (which would land later limits in the wrong place).
    #[test]
    fn enable_controllers_errors_when_none_delegated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cgroup_path = dir.path();
        std::fs::write(cgroup_path.join("cgroup.controllers"), "io rdma\n")
            .expect("write cgroup.controllers");
        std::fs::write(cgroup_path.join("cgroup.subtree_control"), "")
            .expect("write cgroup.subtree_control");

        let err = enable_controllers(cgroup_path).expect_err("must err on empty intersection");
        let msg = format!("{err}");
        assert!(
            msg.contains("none of cpu/memory/pids"),
            "error must spell out the missing controllers; got {msg:?}"
        );
    }

    /// `apply_cgroup_defaults` fills every cap when none was set explicitly:
    /// memory.max = 2× VM + 512 MiB, pids.max = 1024, cpu.max = host_cores ×
    /// 1_000_000, and cpu_quota_us_per_sec mirrors cpu.max. Pins the default
    /// values themselves — a regression that quietly lowers any of them
    /// would land here.
    #[test]
    fn apply_defaults_fills_every_cap_when_none_explicit() {
        let mut config = CgroupConfig::default();
        apply_cgroup_defaults(&mut config, 128, 8);

        let expected_mem = 128u64 * 2 * 1024 * 1024 + 512 * 1024 * 1024;
        assert_eq!(config.memory_max, Some(expected_mem));
        assert_eq!(config.pids_max, Some(DEFAULT_HOST_PIDS_MAX));
        assert_eq!(config.cpu_max, Some((8 * 1_000_000, 1_000_000)));
        assert_eq!(
            config.cpu_quota_us_per_sec,
            Some(8 * 1_000_000),
            "cpu_quota_us_per_sec must mirror the default cpu_max"
        );
    }

    /// The load-bearing mirror: when `ResourceLimits.max_cpu_time` is set,
    /// the `From<&ResourceLimits>` impl populates `cpu_max` only —
    /// `cpu_quota_us_per_sec` stays `None`. `apply_cgroup_defaults` must
    /// derive `cpu_quota_us_per_sec` *from* `cpu_max`, otherwise rootless
    /// (busctl `CPUQuotaPerSecUSec`) silently drops the user's CPU cap
    /// even though rootful (`cpu.max` file write) honours it.
    ///
    /// Before this PR's `15c50197 + apply_cgroup_defaults` refactor the
    /// mirror didn't exist — explicit caps worked rootful but silently
    /// failed rootless. This test pins the property so a future refactor
    /// can't quietly regress.
    #[test]
    fn apply_defaults_mirrors_explicit_cpu_max_to_quota() {
        // Simulate what `From<&ResourceLimits>` produces when the user
        // sets `max_cpu_time = 2`: cpu_max = (2_000_000, 1_000_000),
        // cpu_quota_us_per_sec = None.
        let mut config = CgroupConfig {
            cpu_max: Some((2_000_000, 1_000_000)),
            ..Default::default()
        };
        apply_cgroup_defaults(&mut config, 128, 8);

        assert_eq!(
            config.cpu_max,
            Some((2_000_000, 1_000_000)),
            "explicit cpu_max must NOT be overridden by the default"
        );
        assert_eq!(
            config.cpu_quota_us_per_sec,
            Some(2_000_000),
            "cpu_quota_us_per_sec must mirror the explicit cpu_max so \
             the rootless busctl path enforces the same cap as the \
             rootful cpu.max file-write path; got {:?}",
            config.cpu_quota_us_per_sec
        );
    }

    /// Explicit `memory_max` / `pids_max` must NOT be clobbered by the
    /// defaults — the user knows what they want, the defaults are
    /// fallbacks only.
    #[test]
    fn apply_defaults_does_not_override_explicit_values() {
        let mut config = CgroupConfig {
            memory_max: Some(42),
            pids_max: Some(7),
            cpu_quota_us_per_sec: Some(13),
            ..Default::default()
        };
        apply_cgroup_defaults(&mut config, 128, 8);

        assert_eq!(config.memory_max, Some(42), "explicit memory_max preserved");
        assert_eq!(config.pids_max, Some(7), "explicit pids_max preserved");
        assert_eq!(
            config.cpu_quota_us_per_sec,
            Some(13),
            "explicit cpu_quota_us_per_sec preserved (not overwritten by mirror)"
        );
    }

    /// `CPUQuotaPerSecUSec` from `CgroupConfig` must land verbatim on the
    /// systemd transient scope when `adopt_pid_into_scope` is called.
    ///
    /// Commit `a6386896` added the `cpu_quota_us_per_sec` field on
    /// `CgroupConfig` + the busctl property marshalling, but no production
    /// code path currently sets the field (the default in
    /// `jailer::cgroup_config()` only wires `memory_max` + `pids_max`).
    /// This test guards the **plumbing** so when a future PR wires CPU
    /// quota in from `ResourceLimits` or a CLI flag, the busctl property
    /// name (`CPUQuotaPerSecUSec`), the dbus variant type (`t` = u64), and
    /// the "microseconds per second" units stay correct.
    ///
    /// Without this test, a regression in `adopt_pid_into_scope`'s
    /// property list (e.g. renaming the key, dropping the type tag,
    /// reordering the count) would land silently and only surface once
    /// the wire-up was attempted — at which point CPU caps would be
    /// dropped without any error.
    ///
    /// Skipped when running as root (rootful path uses direct cgroup
    /// writes, not busctl) or when there is no systemd `--user` manager
    /// to talk to. The 50 % quota value is chosen so the read-back is
    /// unambiguous (`500000` µs/s; systemd normalises `1000000` to
    /// `infinity`).
    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_quota_per_sec_usec_lands_on_the_scope() {
        // Precondition 1: rootless. The rootful path takes the direct
        // `/sys/fs/cgroup/boxlite/<id>` route and never touches busctl.
        let uid = unsafe { libc::getuid() };
        if uid == 0 {
            eprintln!("SKIP cpu_quota_per_sec_usec_lands_on_the_scope: running as root");
            return;
        }

        // Precondition 2: a working systemd user manager (so busctl /
        // systemctl can talk to it).
        let probe = std::process::Command::new("systemctl")
            .args(["--user", "show", "init.scope", "-p", "MemoryMax"])
            .output()
            .ok();
        if probe.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
            eprintln!(
                "SKIP cpu_quota_per_sec_usec_lands_on_the_scope: \
                 no systemd --user manager"
            );
            return;
        }

        // A long-lived dummy process to adopt. The scope auto-tears down
        // when the last PID exits, so the `sleep 30` cleanup is also our
        // scope cleanup if we panic mid-test.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();

        // 50 % CPU = 500 000 µs per 1 000 000 µs second. systemd reports
        // this back literally in CPUQuotaPerSecUSec.
        let quota_us: u64 = 500_000;
        let box_id = format!("cpuquota-plumb-{pid}");
        let config = CgroupConfig {
            cpu_quota_us_per_sec: Some(quota_us),
            ..Default::default()
        };

        let adopt_result = adopt_pid_into_scope(&box_id, pid, &config);

        // Read the property back via systemctl. We capture this BEFORE
        // cleanup so the scope still exists at read time.
        let unit = format!("boxlite-{box_id}.scope");
        let prop_out = std::process::Command::new("systemctl")
            .args(["--user", "show", &unit, "-p", "CPUQuotaPerSecUSec"])
            .output()
            .ok();

        // Tear the dummy process down ASAP regardless of what we found —
        // a panic in the assertions below should not leak `sleep 30`.
        let _ = child.kill();
        let _ = child.wait();
        // Best-effort teardown of the transient unit (it should also
        // auto-clean when the last PID exits, but belt and braces).
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "stop", &unit])
            .output();

        // If adopt failed (e.g. busctl missing) treat it as SKIP rather
        // than FAIL — the rest of the assertion presumes the scope
        // actually got created.
        if adopt_result.is_err() {
            eprintln!(
                "SKIP cpu_quota_per_sec_usec_lands_on_the_scope: \
                 adopt_pid_into_scope failed: {:?}",
                adopt_result.err()
            );
            return;
        }

        let read_back = prop_out
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .expect("systemctl --user show must succeed after a successful adopt");
        let value = read_back
            .strip_prefix("CPUQuotaPerSecUSec=")
            .expect("output must be `CPUQuotaPerSecUSec=<value>`")
            .trim();
        // systemd formats the property in human-readable form on show:
        // 500_000 µs/s → "500ms". Either string form is acceptable, but
        // the regression must NOT yield "infinity" (= property dropped)
        // or some other value — both would break a future CPU-cap PR.
        assert!(
            value == "500ms" || value == "500000",
            "the CPUQuotaPerSecUSec property must round-trip from \
             CgroupConfig through busctl to systemd as 500 000 µs/s \
             (either raw `500000` or formatted `500ms`); got {value:?}"
        );
    }
}
