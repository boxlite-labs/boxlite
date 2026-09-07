//! Integration test: rootless host cgroup enforcement.
//!
//! On a rootless host the shim can't migrate itself across the root-owned
//! user.slice into a limited cgroup, so boxlite asks the systemd user manager
//! to adopt the running shim into a transient `boxlite-<id>.scope` carrying
//! `MemoryMax` (2×VM + 512 MiB headroom) and `TasksMax` (1024). This bounds the
//! host RAM / PID blast radius of a single box. Without that adoption the shim
//! stays in the unconstrained login `session-N.scope` (MemoryMax=infinity),
//! which is exactly what this test fails on.
//!
//! Skips when running as root (which uses the direct-cgroup path, not a scope)
//! or when there is no systemd user manager to query.

use assert_cmd::Command;
use boxlite_test_utils::home::PerTestBoxHome;
use std::path::Path;
use std::time::Duration;

fn boxlite(home: &Path, args: &[&str], timeout: Duration) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_boxlite"))
        .arg("--home")
        .arg(home)
        .args(args)
        .timeout(timeout)
        .output()
        .expect("spawn boxlite")
}

struct BoxCleanup {
    home: std::path::PathBuf,
    id: String,
}
impl Drop for BoxCleanup {
    fn drop(&mut self) {
        let _ = boxlite(&self.home, &["rm", "-f", &self.id], Duration::from_secs(30));
    }
}

/// `systemctl --user show <unit> -p <prop>` → the property's value, or None if
/// systemctl/the user manager isn't usable here.
fn systemctl_show(unit: &str, prop: &str) -> Option<String> {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", unit, "-p", prop])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    line.trim()
        .strip_prefix(&format!("{prop}="))
        .map(|v| v.trim().to_string())
}

fn is_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Start a detached 128 MiB alpine box scoped on the systemd user manager, or
/// `None` (with an `eprintln!` SKIP) when the preconditions aren't met (root
/// path or no user manager). Caller is responsible for cleanup of the returned
/// box id.
fn start_scoped_box_or_skip(home: &PerTestBoxHome) -> Option<String> {
    if is_root() {
        eprintln!("SKIP: running as root uses the direct cgroup path, not a systemd scope");
        return None;
    }
    if systemctl_show("init.scope", "MemoryMax").is_none() {
        eprintln!("SKIP: no systemd --user manager to query");
        return None;
    }
    let out = boxlite(
        home.path.as_path(),
        &[
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "128",
            "alpine:latest",
            "sleep",
            "600",
        ],
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "box start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn shim_is_scoped_with_host_memory_and_pids_limits() {
    let home = PerTestBoxHome::new();
    let Some(box_id) = start_scoped_box_or_skip(&home) else {
        return;
    };
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let unit = format!("boxlite-{box_id}.scope");
    // 2×128 MiB VM + 512 MiB headroom — the documented host memory cap.
    let expected_mem = (128u64 * 2 * 1024 * 1024) + (512 * 1024 * 1024);

    let mem = systemctl_show(&unit, "MemoryMax").unwrap_or_default();
    let tasks = systemctl_show(&unit, "TasksMax").unwrap_or_default();

    assert_eq!(
        mem,
        expected_mem.to_string(),
        "shim must be scoped with MemoryMax={expected_mem}; got {mem:?} for {unit} \
         (an unscoped shim reports MemoryMax=infinity — limits not enforced)"
    );
    assert_eq!(
        tasks, "1024",
        "shim scope must cap TasksMax at 1024; got {tasks:?} for {unit}"
    );

    // The cap is meaningless if the shim was never moved into the scope. A
    // regression that breaks `StartTransientUnit` (or its PID-adoption arg)
    // could leave the scope existing with the right cap but EMPTY, while the
    // shim stays in the unconstrained login `session-N.scope`. MemoryCurrent
    // accounts every byte charged inside the scope; it is `0` exactly when no
    // process is enrolled.
    let mem_current_str = systemctl_show(&unit, "MemoryCurrent").unwrap_or_default();
    let mem_current: u64 = mem_current_str.parse().unwrap_or(0);
    assert!(
        mem_current > 0,
        "scope {unit} must have processes enrolled in it — MemoryCurrent={mem_current_str:?} \
         means the cap exists but the shim PID never got adopted into the scope"
    );
}

/// `cgroup_config()` defaults `cpu_quota_us_per_sec` to `host_cores ×
/// 1_000_000` µs/s (mirroring how `memory.max` defaults to `2× VM` and
/// `pids.max` to `1024`) so a rootless deployment without explicit
/// `ResourceLimits.max_cpu_time` is no longer silently uncapped on CPU.
///
/// Before this PR's CPU-default change the property landed as `infinity`
/// — a runaway shim could pin every core forever. This test pins the new
/// behaviour end-to-end: start a box rootless, look up
/// `CPUQuotaPerSecUSec` on the scope, assert it matches the host's online
/// core count (`std::thread::available_parallelism()`).
///
/// Two-sided naturally: rip the `cpu_quota_us_per_sec` default block out
/// of `jailer::cgroup_config()` and this test fails with `got "infinity"`
/// — proving the default is the load-bearing piece. Skipped under root
/// (rootful uses `cpu.max` file-write, a different code path covered by
/// `rootful_host_cgroup_is_created_with_limits`) or no systemd
/// user-manager.
#[test]
fn shim_scope_caps_cpu_quota_at_host_core_count() {
    let home = PerTestBoxHome::new();
    let Some(box_id) = start_scoped_box_or_skip(&home) else {
        return;
    };
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let unit = format!("boxlite-{box_id}.scope");
    let raw = systemctl_show(&unit, "CPUQuotaPerSecUSec").unwrap_or_default();

    assert!(
        raw != "infinity",
        "CPUQuotaPerSecUSec on {unit} must NOT be `infinity` — the \
         rootless default cap was silently dropped. Got: {raw:?}"
    );

    let cores = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .unwrap_or(1);
    let expected_us = cores.saturating_mul(1_000_000);

    // systemd `show` formats the value back as `<N>s` for whole-second
    // multiples and as `<N>` µs otherwise. Accept either form.
    let formatted_s = format!("{cores}s");
    let formatted_us = expected_us.to_string();
    assert!(
        raw == formatted_s || raw == formatted_us,
        "CPUQuotaPerSecUSec must default to host_cores × 1s = {cores}s \
         (= {expected_us} µs/s); systemd reported {raw:?} on {unit}. \
         Did `jailer::cgroup_config()` lose its CPU default branch?"
    );
}

#[test]
fn shim_scope_is_cleaned_up_when_box_is_removed() {
    let home = PerTestBoxHome::new();
    let Some(box_id) = start_scoped_box_or_skip(&home) else {
        return;
    };
    let unit = format!("boxlite-{box_id}.scope");
    // A safety-net cleanup in case the test panics before reaching the
    // explicit `rm` below — keeps the per-test home guard from firing on a
    // live shim. The explicit rm runs first; this is idempotent (`rm -f`).
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    // Sanity: the scope is active before we tear the box down.
    let active_before = systemctl_show(&unit, "ActiveState").unwrap_or_default();
    assert_eq!(
        active_before, "active",
        "scope {unit} must be active before rm; got {active_before:?}"
    );

    // Remove the box: the shim dies, the scope's last PID leaves, systemd
    // should transition the transient unit out of `active` (and shortly after
    // garbage-collect it). A scope that stays `active` is a leak.
    let rm = boxlite(
        home.path.as_path(),
        &["rm", "-f", &box_id],
        Duration::from_secs(60),
    );
    assert!(
        rm.status.success(),
        "rm failed: {}",
        String::from_utf8_lossy(&rm.stderr)
    );

    // Poll briefly: systemd needs a moment to notice the scope is empty.
    let timeout = Duration::from_secs(10);
    let start = std::time::Instant::now();
    let final_state = loop {
        let s = systemctl_show(&unit, "ActiveState").unwrap_or_default();
        if s != "active" || start.elapsed() >= timeout {
            break s;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_ne!(
        final_state,
        "active",
        "scope {unit} must not stay `active` after the box is removed (leaked scope); \
         ActiveState still 'active' after {}s",
        timeout.as_secs()
    );
}

// ============================================================================
// Review-pass additions: the rootful direct-write path (half of production!),
// concurrent boxes, and restart. Original two tests only covered the
// rootless systemd-scope path.
// ============================================================================

/// **Rootful production path** — half of production (CI, server deployments)
/// runs `boxlite` as root, taking the `/sys/fs/cgroup/boxlite/<id>` direct-
/// write path instead of the rootless systemd-scope. The rootless tests
/// above `SKIP` for root, leaving this path completely unguarded.
#[test]
fn rootful_host_cgroup_is_created_with_limits() {
    if !is_root() {
        eprintln!("SKIP: this test exercises the root-only direct-write path");
        return;
    }
    let home = PerTestBoxHome::new();
    let out = boxlite(
        home.path.as_path(),
        &[
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "128",
            "alpine:latest",
            "sleep",
            "600",
        ],
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "box start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let box_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let cg = std::path::PathBuf::from("/sys/fs/cgroup/boxlite").join(&box_id);
    assert!(
        cg.exists(),
        "rootful direct-write path: cgroup dir {} must exist",
        cg.display()
    );
    let mem = std::fs::read_to_string(cg.join("memory.max"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let pids = std::fs::read_to_string(cg.join("pids.max"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let expected_mem = (128u64 * 2 * 1024 * 1024) + (512 * 1024 * 1024);
    assert_eq!(
        mem,
        expected_mem.to_string(),
        "memory.max must be 2×VM + 512 MiB; got {mem:?} in {}",
        cg.display()
    );
    assert_eq!(
        pids,
        "1024",
        "pids.max must be 1024; got {pids:?} in {}",
        cg.display()
    );
}

/// Three boxes started together must each get a distinct host cgroup unit
/// (rootless `boxlite-<idN>.scope` or rootful `/sys/fs/cgroup/boxlite/<idN>`),
/// no collisions or shared caps. A regression that derived the scope name
/// from something shared would fail this.
#[test]
fn concurrent_boxes_get_distinct_host_cgroups() {
    let home = PerTestBoxHome::new();
    if !is_root() && systemctl_show("init.scope", "MemoryMax").is_none() {
        eprintln!("SKIP: rootless without systemd --user manager");
        return;
    }

    let mut ids: Vec<String> = Vec::new();
    let mut cleanups: Vec<BoxCleanup> = Vec::new();
    for _ in 0..3 {
        let out = boxlite(
            home.path.as_path(),
            &[
                "--registry",
                "docker.m.daocloud.io",
                "run",
                "-d",
                "--memory",
                "64",
                "alpine:latest",
                "sleep",
                "300",
            ],
            Duration::from_secs(300),
        );
        assert!(
            out.status.success(),
            "box start failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        cleanups.push(BoxCleanup {
            home: home.path.clone(),
            id: id.clone(),
        });
        ids.push(id);
    }

    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "boxlite must generate distinct ids per box; got {ids:?}"
    );

    for id in &ids {
        if is_root() {
            let cg = std::path::PathBuf::from("/sys/fs/cgroup/boxlite").join(id);
            assert!(
                cg.exists(),
                "rootful: cgroup dir {} for box {id} must exist",
                cg.display()
            );
        } else if let Some(state) = systemctl_show(&format!("boxlite-{id}.scope"), "ActiveState") {
            assert_eq!(
                state, "active",
                "scope for box {id} must be active; got {state:?}"
            );
        }
    }
}

/// Stop → start: the new shim's scope/cgroup must be re-created cleanly.
/// A stale unit from the previous run would make `StartTransientUnit` fail
/// with `mode=fail` on the second start (rootless), or `mkdir` fail with
/// EEXIST followed by no re-application of limits (rootful).
#[test]
fn host_cgroup_is_reset_across_stop_and_start() {
    let home = PerTestBoxHome::new();
    if !is_root() && systemctl_show("init.scope", "MemoryMax").is_none() {
        eprintln!("SKIP: rootless without systemd --user manager");
        return;
    }

    let out = boxlite(
        home.path.as_path(),
        &[
            "--registry",
            "docker.m.daocloud.io",
            "run",
            "-d",
            "--memory",
            "64",
            "alpine:latest",
            "sleep",
            "300",
        ],
        Duration::from_secs(300),
    );
    assert!(
        out.status.success(),
        "box start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let box_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let stop = boxlite(
        home.path.as_path(),
        &["stop", &box_id],
        Duration::from_secs(60),
    );
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    // Brief poll so systemd reaps the empty scope / kernel empties the cgroup.
    std::thread::sleep(Duration::from_secs(2));

    let start = boxlite(
        home.path.as_path(),
        &["start", &box_id],
        Duration::from_secs(120),
    );
    assert!(
        start.status.success(),
        "start (after stop) failed — likely a stale unit/cgroup from the previous \
         run blocking re-creation: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    if is_root() {
        let cg = std::path::PathBuf::from("/sys/fs/cgroup/boxlite").join(&box_id);
        assert!(
            cg.exists(),
            "after restart, rootful cgroup dir {} must exist again",
            cg.display()
        );
    } else if let Some(state) = systemctl_show(&format!("boxlite-{box_id}.scope"), "ActiveState") {
        assert_eq!(
            state, "active",
            "after restart, scope must be active again; got {state:?}"
        );
    }
}
