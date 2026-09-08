//! Integration tests for jailer enforcement.
//!
//! Verifies:
//! 1. Jailer is enabled by default on macOS (disabled by default on Linux)
//! 2. Boxes start and execute correctly with jailer enabled (regression guard)
//! 3. Explicitly disabling the jailer still works
//! 4. On Linux: bwrap creates isolated mount/user namespaces

mod common;

use boxlite::runtime::advanced_options::{AdvancedBoxOptions, SecurityOptions};
use boxlite::runtime::options::{BoxOptions, NetworkSpec};
use common::box_test::BoxTestBase;
use std::path::PathBuf;

// ============================================================================
// JAILER-SPECIFIC HELPERS
// ============================================================================

#[cfg(target_os = "macos")]
const MACOS_UNIX_SOCKET_PATH_MAX: usize = 104;

fn jailer_test_home_base_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".boxlite-it")
}

#[cfg(target_os = "macos")]
fn assert_macos_socket_path_budget(home_dir: &std::path::Path) {
    let probe = home_dir
        .join("boxes")
        .join("12345678-1234-1234-1234-123456789abc")
        .join("sockets")
        .join("box.sock");
    let probe_len = probe.to_string_lossy().len();
    let budget = MACOS_UNIX_SOCKET_PATH_MAX - 1;
    assert!(
        probe_len <= budget,
        "Jailer test home base is too long for macOS Unix socket paths \
         (probe={}, len={}, budget={}). Use a shorter base path than {}",
        probe.display(),
        probe_len,
        budget,
        home_dir.display()
    );
}

/// Per-test home for jailer tests under `~/.boxlite-it`.
///
/// Uses a short base path to satisfy macOS 104-char Unix socket path limit.
/// Cleanup: `PerTestBoxHome` (owned by `BoxTestBase` after `.home` is moved)
/// handles per-test TempDir removal via Drop. The base dir `~/.boxlite-it`
/// is left in place (shared across test runs).
struct JailerHome {
    home: boxlite_test_utils::home::PerTestBoxHome,
}

impl JailerHome {
    fn new() -> Self {
        let base = jailer_test_home_base_dir();
        std::fs::create_dir_all(&base).expect("create jailer test home base");
        let home = boxlite_test_utils::home::PerTestBoxHome::new_in(
            base.to_str().expect("base path UTF-8"),
        );

        #[cfg(target_os = "macos")]
        assert_macos_socket_path_budget(&home.path);
        #[cfg(target_os = "macos")]
        {
            let canonical = home
                .path
                .canonicalize()
                .unwrap_or_else(|_| home.path.clone());
            assert!(
                !canonical.starts_with("/private/tmp"),
                "jailer tests must not use /private/tmp as home_dir: {}",
                canonical.display()
            );
        }

        Self { home }
    }
}

fn jailer_enabled_options() -> BoxOptions {
    let mut advanced = AdvancedBoxOptions::default();
    advanced.security = SecurityOptions {
        jailer_enabled: true,
        ..SecurityOptions::default()
    };
    BoxOptions {
        advanced,
        ..common::alpine_opts()
    }
}

fn jailer_disabled_options() -> BoxOptions {
    let mut advanced = AdvancedBoxOptions::default();
    advanced.security = SecurityOptions {
        jailer_enabled: false,
        ..SecurityOptions::default()
    };
    BoxOptions {
        advanced,
        ..common::alpine_opts()
    }
}

#[cfg(target_os = "macos")]
fn with_sandbox_profile(mut options: BoxOptions, profile_path: std::path::PathBuf) -> BoxOptions {
    options.advanced.security.sandbox_profile = Some(profile_path);
    options
}

#[cfg(target_os = "macos")]
fn sandbox_exec_available() -> bool {
    std::path::Path::new("/usr/bin/sandbox-exec").exists()
}

#[cfg(target_os = "macos")]
fn sbpl_escape(path: &std::path::Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(target_os = "macos")]
fn write_deny_boxes_profile(home_dir: &std::path::Path) -> std::path::PathBuf {
    let raw_boxes = home_dir.join("boxes");
    let canonical_boxes = raw_boxes
        .canonicalize()
        .unwrap_or_else(|_| raw_boxes.clone());

    let mut deny_rules = vec![
        format!(
            "(deny file-read* (subpath \"{}\"))",
            sbpl_escape(raw_boxes.as_path())
        ),
        format!(
            "(deny file-write* (subpath \"{}\"))",
            sbpl_escape(raw_boxes.as_path())
        ),
    ];

    if canonical_boxes != raw_boxes {
        deny_rules.push(format!(
            "(deny file-read* (subpath \"{}\"))",
            sbpl_escape(canonical_boxes.as_path())
        ));
        deny_rules.push(format!(
            "(deny file-write* (subpath \"{}\"))",
            sbpl_escape(canonical_boxes.as_path())
        ));
    }

    let profile = format!("(version 1)\n(allow default)\n{}\n", deny_rules.join("\n"));

    let profile_path = home_dir.join("deny-boxes.sbpl");
    std::fs::write(&profile_path, profile).expect("Failed to write deny profile");
    profile_path
}

// ============================================================================
// DEFAULT CONFIGURATION TESTS
// ============================================================================

/// Verify SecurityOptions::default() enables the jailer (secure by default).
///
/// Since #652 the default profile is fully enabled on every supported
/// platform, so `jailer_enabled` is `true` on both macOS and Linux.
#[test]
fn default_security_options_enable_jailer_on_supported_platforms() {
    let opts = SecurityOptions::default();

    assert!(
        opts.jailer_enabled,
        "Jailer should be enabled by default (secure by default)"
    );
}

/// Verify SecurityOptions::disabled() always disables the jailer.
#[test]
fn development_mode_disables_jailer() {
    let opts = SecurityOptions::disabled();
    assert!(
        !opts.jailer_enabled,
        "Development mode must always disable the jailer"
    );
}

/// Verify SecurityOptions::enabled() enables the jailer on Linux/macOS.
#[test]
fn standard_mode_enables_jailer() {
    let opts = SecurityOptions::enabled();

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(
        opts.jailer_enabled,
        "Standard mode should enable jailer on Linux/macOS"
    );
}

// ============================================================================
// INTEGRATION TESTS: Jailer enabled regression guard
// ============================================================================

/// Box with jailer enabled starts and executes commands successfully.
#[tokio::test]
async fn jailer_enabled_box_starts_and_executes() {
    let jh = JailerHome::new();
    let t = BoxTestBase::with_home(jh.home, jailer_enabled_options()).await;
    t.bx.start().await.unwrap();

    let out = t.exec_stdout("echo", &["jailer-test"]).await;
    assert!(
        out.contains("jailer-test"),
        "Command should succeed with jailer enabled"
    );
}

/// #1072: dropping the jailer's own network grants must not take the shim's
/// AF_UNIX control plane with it.
///
/// `security.network_enabled=false` gates host-side grants only, but on macOS
/// those grants carried the profile's sole `network-bind`/`network-outbound`
/// rules — which is how the shim binds `box.sock` and dials `ready.sock`. The
/// box therefore died on `bind: operation not permitted` about a millisecond
/// after start, for any network mode. Paired here with `NetworkSpec::Disabled`
/// because that is the only combination config validation still accepts.
#[tokio::test]
async fn host_network_grants_off_box_starts_and_executes() {
    let jh = JailerHome::new();
    let mut advanced = AdvancedBoxOptions::default();
    advanced.security = SecurityOptions {
        jailer_enabled: true,
        network_enabled: false,
        ..SecurityOptions::default()
    };
    let options = BoxOptions {
        advanced,
        network: NetworkSpec::Disabled,
        ..common::alpine_opts()
    };
    let t = BoxTestBase::with_home(jh.home, options).await;
    t.bx.start().await.unwrap();

    let out = t.exec_stdout("echo", &["no-net-grants"]).await;
    assert!(
        out.contains("no-net-grants"),
        "Box must start without the jailer's network grants"
    );
}

/// Box with jailer explicitly disabled still works (development mode).
#[tokio::test]
async fn jailer_disabled_box_starts_and_executes() {
    let jh = JailerHome::new();
    let t = BoxTestBase::with_home(jh.home, jailer_disabled_options()).await;
    t.bx.start().await.unwrap();

    let out = t.exec_stdout("echo", &["no-jailer-test"]).await;
    assert!(
        out.contains("no-jailer-test"),
        "Command should succeed with jailer disabled"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn jailer_enabled_custom_profile_deny_boxes_subpath_blocks_start() {
    if !sandbox_exec_available() {
        eprintln!("Skipping: /usr/bin/sandbox-exec not available");
        return;
    }

    let jh = JailerHome::new();
    let profile_path = write_deny_boxes_profile(&jh.home.path);
    let t = BoxTestBase::with_home(
        jh.home,
        with_sandbox_profile(jailer_enabled_options(), profile_path),
    )
    .await;

    let box_id = t.bx.id().clone();
    let start_result =
        tokio::time::timeout(std::time::Duration::from_secs(600), t.bx.start()).await;

    let start_result = match start_result {
        Ok(result) => result,
        Err(_) => {
            panic!("start() timed out while waiting for sandbox denial");
        }
    };
    assert!(
        start_result.is_err(),
        "Expected start to fail with deny profile for boxes subpath"
    );

    let stderr_path = t
        .home_dir()
        .join("boxes")
        .join(box_id.as_str())
        .join("shim.stderr");
    assert!(
        stderr_path.exists(),
        "shim.stderr should exist after denied startup: {}",
        stderr_path.display()
    );

    let stderr = std::fs::read_to_string(&stderr_path).expect("Should read shim.stderr");
    let stderr_lower = stderr.to_lowercase();
    // "file exists" is valid deny evidence: when the sandbox blocks stat() on a
    // pre-created directory, Rust's create_dir_all can't verify the existing path
    // is a directory and surfaces the original EEXIST from mkdir instead of Ok(()).
    // Without the sandbox, create_dir_all handles existing directories gracefully.
    let has_deny_evidence = stderr_lower.contains("operation not permitted")
        || stderr_lower.contains("sandbox")
        || stderr_lower.contains("deny")
        || stderr_lower.contains("file exists");
    assert!(
        has_deny_evidence,
        "Expected sandbox deny evidence in shim.stderr, got:\n{}",
        stderr
    );
    // Drop: BoxTestBase -> RuntimeImpl::Drop stops non-detached boxes,
    //        PerTestBoxHome -> TempDir cleans up per-test dir.
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn jailer_disabled_with_same_profile_still_starts() {
    if !sandbox_exec_available() {
        eprintln!("Skipping: /usr/bin/sandbox-exec not available");
        return;
    }

    let jh = JailerHome::new();
    let profile_path = write_deny_boxes_profile(&jh.home.path);
    let t = BoxTestBase::with_home(
        jh.home,
        with_sandbox_profile(jailer_disabled_options(), profile_path),
    )
    .await;
    t.bx.start().await.unwrap();

    let out = t
        .exec_stdout("echo", &["profile-ignored-with-jailer-disabled"])
        .await;
    assert!(
        out.contains("profile-ignored-with-jailer-disabled"),
        "Control case should start and execute"
    );
}

// ============================================================================
// LINUX-ONLY: Namespace isolation enforcement
// ============================================================================

/// Find a descendant of `root` whose mount namespace differs from `host`.
///
/// `shim.pid` records the *outer* bwrap launcher, which stays in the host mount
/// namespace. bwrap runs with `--unshare-pid`, so it forks: the sandboxed
/// processes — the inner bwrap and the shim itself (which runs under libkrun,
/// so its `comm` shows as "libkrun VM", not "boxlite-shim") — live in a new
/// namespace below it. Walk `/proc` parent links from `root` and return the
/// first descendant that is actually isolated. Returns `None` only if no
/// descendant left the host namespace, i.e. isolation genuinely failed.
#[cfg(target_os = "linux")]
fn isolated_descendant(root: u32, host: &std::path::Path) -> Option<u32> {
    // A failure to read /proc is a broken test environment, not "no isolation";
    // fail fast with the real cause rather than reporting an isolation failure.
    let procs: Vec<(u32, u32)> = std::fs::read_dir("/proc")
        .expect("read /proc to walk the process tree")
        .flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
            let ppid = status
                .lines()
                .find_map(|l| l.strip_prefix("PPid:")?.trim().parse::<u32>().ok())?;
            Some((pid, ppid))
        })
        .collect();

    let mut stack = vec![root];
    while let Some(cur) = stack.pop() {
        for (pid, ppid) in &procs {
            if *ppid == cur {
                let isolated = std::fs::read_link(format!("/proc/{pid}/ns/mnt"))
                    .is_ok_and(|ns| ns.as_path() != host);
                if isolated {
                    return Some(*pid);
                }
                stack.push(*pid);
            }
        }
    }
    None
}

/// On Linux, verify bwrap creates an isolated mount namespace for the shim.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn jailer_creates_isolated_mount_namespace() {
    let jh = JailerHome::new();
    let t = BoxTestBase::with_home(jh.home, jailer_enabled_options()).await;
    t.bx.start().await.unwrap();

    // Start a long-running command so the shim stays alive
    let _execution =
        t.bx.exec(boxlite::BoxCommand::new("sleep").arg("30"))
            .await
            .unwrap();

    // Read the shim's PID
    let pid_file = t
        .home_dir()
        .join("boxes")
        .join(t.bx.id().as_str())
        .join("shim.pid");
    let recorded_pid = boxlite::util::PidFileReader::at(&pid_file)
        .read()
        .map(|r| r.pid)
        .expect("Should read shim PID file");

    let self_mnt_ns =
        std::fs::read_link("/proc/self/ns/mnt").expect("Should read own mount namespace");

    // The recorded pid is the outer bwrap launcher, which shares the test's
    // (host) mount namespace; the sandboxed processes are its descendants in a
    // new namespace. Verify at least one descendant is actually isolated.
    let isolated = isolated_descendant(recorded_pid, &self_mnt_ns);

    assert!(
        isolated.is_some(),
        "bwrap should place the sandboxed process tree in a different mount \
         namespace than the test, but recorded pid {recorded_pid} and all of its \
         descendants share the test namespace {self_mnt_ns:?} (bwrap isolation inactive)"
    );
}
