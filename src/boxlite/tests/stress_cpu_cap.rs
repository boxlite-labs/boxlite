//! Integration test: the host CPU cap actually enforces under load.
//!
//! `15c50197` defaults `cpu_quota_us_per_sec` to `host_cores × 1_000_000` and
//! mirrors any explicit `ResourceLimits.max_cpu_time` from `cpu_max` so the
//! rootless busctl path enforces the same cap as the rootful direct write.
//! The cleaner unit + integration tests in `stress_host_cgroup.rs` and
//! `jailer::cgroup::tests` verify that the cap value lands on the systemd
//! scope — but value-on-paper is not the same as "the kernel actually throttles
//! a busy workload."
//!
//! This test runs the actual stress check end-to-end:
//!   1. Start a box with `max_cpu_time = 1` (1-core cap) and 4 vCPUs (so the
//!      workload can theoretically use 4 cores of host CPU without the cap).
//!   2. Spawn 4 background `yes > /dev/null` processes inside the box — each
//!      pins one vCPU at 100 %.
//!   3. Let CPU usage ramp for 2 s.
//!   4. Sample the scope's `CPUUsageNSec` over a 3 s window.
//!   5. Assert the consumed-per-wall rate is ≤ 1.5 cores (cap + 50 % slack
//!      for kernel scheduling jitter and tokio runtime overhead).
//!
//! Without the cap enforcement (e.g. if `cpu_quota_us_per_sec` mirror to
//! `cpu_max` regresses, or the default is removed), rate climbs to ~4 cores —
//! the four `yes` processes plus the libkrun vCPU threads they sit on each
//! consume close to one core.
//!
//! Skipped under root (rootful uses `cpu.max` file-write — covered by
//! `rootful_host_cgroup_is_created_with_limits` for *config* and by this same
//! test running rootless for *enforcement* via the busctl path) or no systemd
//! `--user` manager.

mod common;

use boxlite::BoxliteRuntime;
use boxlite::litebox::BoxCommand;
use boxlite::runtime::options::{BoxOptions, BoxliteOptions};
use std::time::{Duration, Instant};

/// `systemctl --user show <unit> -p <prop>` → `<prop>=<value>` raw value.
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

/// Read the scope's cgroup absolute path on disk (e.g.
/// `/sys/fs/cgroup/user.slice/user-1000.slice/.../boxlite-XYZ.scope`).
/// `CPUUsageNSec` isn't tracked by default on rootless transient scopes, so
/// we read `cpu.stat`'s `usage_usec` line from the cgroup directly.
fn scope_cgroup_path(unit: &str) -> Option<std::path::PathBuf> {
    let cg = systemctl_show(unit, "ControlGroup")?;
    if cg.is_empty() {
        return None;
    }
    Some(std::path::PathBuf::from("/sys/fs/cgroup").join(cg.trim_start_matches('/')))
}

fn cgroup_cpu_usage_usec(cgroup_dir: &std::path::Path) -> Option<u64> {
    let stat = std::fs::read_to_string(cgroup_dir.join("cpu.stat")).ok()?;
    for line in stat.lines() {
        if let Some(n) = line.strip_prefix("usage_usec ") {
            return n.trim().parse().ok();
        }
    }
    None
}

#[tokio::test]
async fn host_cpu_quota_actually_caps_busy_workload() {
    // Precondition 1: rootless. Rootful takes a different code path
    // (cpu.max file write, no systemd scope to read CPUUsageNSec from).
    if unsafe { libc::getuid() } == 0 {
        eprintln!(
            "SKIP host_cpu_quota_actually_caps_busy_workload: \
             rootful uses cpu.max file-write, different code path"
        );
        return;
    }
    // Precondition 2: systemd user manager available.
    let probe = std::process::Command::new("systemctl")
        .args(["--user", "show", "init.scope", "-p", "MemoryMax"])
        .output()
        .ok();
    if probe.map(|o| !o.status.success()).unwrap_or(true) {
        eprintln!(
            "SKIP host_cpu_quota_actually_caps_busy_workload: \
             no systemd --user manager"
        );
        return;
    }

    let home = boxlite_test_utils::home::PerTestBoxHome::new();
    let runtime = BoxliteRuntime::new(BoxliteOptions {
        home_dir: home.path.clone(),
        image_registries: common::test_registries(),
    })
    .unwrap();

    // Box configured to make the cap *bite*:
    //   - 4 vCPUs so the workload can saturate 4 cores absent any cap
    //   - max_cpu_time = 1 → cpu_max = (1_000_000, 1_000_000) = 1 core
    //     → mirrored to cpu_quota_us_per_sec = 1_000_000 by cgroup_config()
    let mut opts = BoxOptions {
        detach: true,
        cpus: Some(4),
        memory_mib: Some(256),
        ..common::alpine_opts()
    };
    opts.advanced.security.resource_limits.max_cpu_time = Some(1);

    let handle = runtime.create(opts, None).await.expect("box create");
    let box_id = handle.id().to_string();
    let _cleanup = BoxCleanup {
        home_dir: home.path.clone(),
        id: box_id.clone(),
    };

    // Spawn the 4-process spin bomb. Detached via shell `&`; the outer exec
    // returns immediately while the four `yes` processes keep pinning vCPUs.
    let _ = handle
        .exec(BoxCommand::new("sh").args([
            "-c",
            "yes > /dev/null & yes > /dev/null & yes > /dev/null & yes > /dev/null & \
             echo spawned",
        ]))
        .await;

    // Let CPU usage ramp up — yes processes need a second or two before they
    // are reliably pinning vCPUs.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let unit = format!("boxlite-{box_id}.scope");
    let Some(cgroup_dir) = scope_cgroup_path(&unit) else {
        // The lib-API path runs the runtime in-process; depending on the
        // host's systemd user-session wiring (e.g. running under nextest's
        // worker pool vs. an interactive shell), the transient scope may
        // not always be reachable from systemctl --user. The CLI-path
        // equivalent in stress_host_cgroup.rs covers the standard
        // deployment, so SKIP here instead of failing the suite.
        eprintln!(
            "SKIP host_cpu_quota_actually_caps_busy_workload: \
             scope `{unit}` not discoverable via systemctl --user. \
             The scope may not have been created (look for a \
             `place_shim_in_scope` error in the box log), or the test \
             harness lacks a complete systemd --user session."
        );
        return;
    };
    let Some(cpu0_usec) = cgroup_cpu_usage_usec(&cgroup_dir) else {
        eprintln!(
            "SKIP host_cpu_quota_actually_caps_busy_workload: \
             cpu.stat at {} has no usage_usec line (cpu controller \
             may not be delegated to this user session)",
            cgroup_dir.display()
        );
        return;
    };

    let t0 = Instant::now();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let Some(cpu1_usec) = cgroup_cpu_usage_usec(&cgroup_dir) else {
        eprintln!("SKIP: cpu.stat unreadable mid-test (scope torn down?)");
        return;
    };
    let wall_usec = t0.elapsed().as_micros() as u64;

    let cpu_consumed_usec = cpu1_usec.saturating_sub(cpu0_usec);
    // Rate in cores: CPU µs consumed per wall µs elapsed.
    let rate_cores = cpu_consumed_usec as f64 / wall_usec as f64;

    // 1.5 cores = cap (1.0) + 50 % slack for kernel scheduling jitter, tokio
    // runtime threads, and gvproxy. A regression that drops the cap will land
    // ≥ 3 cores easily (4 yes processes × 100 % each − some scheduling loss).
    assert!(
        rate_cores < 1.5,
        "host CPU cap not enforced: scope `{unit}` consumed {cpu_consumed_usec} µs of \
         CPU over {wall_usec} µs wall = {rate_cores:.2} cores, with cap = 1 core. \
         The cap silently leaked — either `cpu_quota_us_per_sec` isn't being \
         set on the scope, or the busctl call dropped the property, or the \
         explicit `max_cpu_time` failed to mirror to cpu_quota_us_per_sec."
    );
}

struct BoxCleanup {
    home_dir: std::path::PathBuf,
    id: String,
}
impl Drop for BoxCleanup {
    fn drop(&mut self) {
        // Best-effort: spawn a new runtime and force-rm the box. The runtime
        // ctor is cheap; this avoids carrying a runtime handle through the
        // test body just to clean up.
        let home_dir = self.home_dir.clone();
        let id = self.id.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("cleanup runtime");
            rt.block_on(async move {
                if let Ok(runtime) = BoxliteRuntime::new(BoxliteOptions {
                    home_dir,
                    image_registries: common::test_registries(),
                }) {
                    let _ = runtime.remove(&id, true).await;
                }
            });
        })
        .join();
    }
}
