//! Integration tests: the box's host cgroup is delegated and its limits land.
//!
//! These start a real box and read the box's own cgroup out of `/sys/fs/cgroup`,
//! which is the only way to observe the property that matters — the unit tests
//! next to `enable_controllers` verify that the right *string* is written to the
//! right filename, not that a running box ends up confined.
//!
//! The chain each test closes:
//!
//! ```text
//! setup_cgroup
//!   └─ enable_controllers → parent's cgroup.subtree_control
//!        └─ kernel materialises pids.max/memory.max in the child cgroup
//!             └─ apply_limits writes the value
//!                  └─ THIS is what the tests below read back
//! ```
//!
//! Skip conditions are deliberate and reported, because a silent skip here reads
//! exactly like a pass. Linux-only by nature; on macOS there is no cgroupfs and
//! every test reports SKIP.

use assert_cmd::Command;
use boxlite_test_utils::home::PerTestBoxHome;
use std::path::{Path, PathBuf};
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
    home: PathBuf,
    id: String,
}
impl Drop for BoxCleanup {
    fn drop(&mut self) {
        let _ = boxlite(&self.home, &["rm", "-f", &self.id], Duration::from_secs(30));
    }
}

fn uid() -> u32 {
    // Avoids a libc dependency in the test crate; `id -u` is in every image the
    // CI hosts run.
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(u32::MAX)
}

/// The directory boxlite puts box cgroups under, mirroring `jailer::cgroup`'s
/// `get_cgroup_base()`: the cgroupfs root when root, otherwise the systemd user
/// manager's delegated subtree.
fn boxlite_cgroup_parent() -> Option<PathBuf> {
    let root = Path::new("/sys/fs/cgroup");
    if !root.join("cgroup.controllers").exists() {
        return None; // no cgroup v2
    }
    if uid() == 0 {
        return Some(root.join("boxlite"));
    }
    let user_scope = root
        .join("user.slice")
        .join(format!("user-{}.slice", uid()))
        .join(format!("user@{}.service", uid()));
    user_scope
        .join("cgroup.controllers")
        .exists()
        .then(|| user_scope.join("boxlite"))
}

/// `Some(parent)` when this host can actually exercise the host-cgroup path, or
/// `None` after printing why not.
fn cgroup_parent_or_skip(what: &str) -> Option<PathBuf> {
    let Some(parent) = boxlite_cgroup_parent() else {
        eprintln!("SKIP {what}: no cgroup v2 (or no systemd user manager for this uid)");
        return None;
    };
    // Writability is what decides whether boxlite can create the parent at all.
    let probe = parent.with_file_name(format!("boxlite-writeprobe-{}", std::process::id()));
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            Some(parent)
        }
        Err(e) => {
            eprintln!(
                "SKIP {what}: cannot create cgroups under {}: {e}",
                parent.display()
            );
            None
        }
    }
}

fn start_box(home: &PerTestBoxHome) -> String {
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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The whole chain, end to end: a box that asked for nothing still ends up with
/// the default per-box process cap actually written into its own cgroup.
///
/// Reading `pids.max` back proves three things at once that are otherwise only
/// inferred — the parent was delegated, the kernel therefore materialised the
/// controller's interface file in the child, and `apply_limits` wrote the value.
///
/// Not discriminating for the intersection half of this change on a host where
/// all of cpu/memory/pids are delegated: there the intersection equals the full
/// set and the pre-fix code would also succeed. It *is* the assertion that fails
/// if delegation regresses in any way.
#[test]
fn box_cgroup_has_the_default_pids_limit_applied() {
    let Some(parent) = cgroup_parent_or_skip("box_cgroup_has_the_default_pids_limit_applied")
    else {
        return;
    };

    let home = PerTestBoxHome::new();
    let box_id = start_box(&home);
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let pids_max = parent.join(&box_id).join("pids.max");
    let value = std::fs::read_to_string(&pids_max).unwrap_or_else(|e| {
        panic!(
            "{} must exist and be readable: {e}. An absent file means the parent's \
             cgroup.subtree_control never got `pids`, so the kernel never created the \
             interface file and the limit was never enforced.",
            pids_max.display()
        )
    });

    assert_eq!(
        value.trim(),
        "1024",
        "default host pids cap must be applied to the box cgroup at {}",
        pids_max.display()
    );
}

/// The half of this change that is a control-flow fix rather than a string fix.
///
/// `enable_controllers` used to run only inside `if !boxlite_cgroup.exists()`.
/// Creating the directory and delegating into it are separate steps, so a
/// failure between them leaves the parent present with an empty
/// `subtree_control`; every later box then took the "already exists" branch,
/// skipped delegation, and got a cgroup with no controller interface files —
/// for the life of the host, since cgroupfs is tmpfs.
///
/// This reproduces that stuck state directly and asserts the next box repairs
/// it. Unlike the test above, it is discriminating on **any** cgroup v2 host,
/// including one where all controllers are delegated, and it is the only path
/// that triggers on a root runner.
///
/// Two-sided: move the `enable_controllers` call back inside the `if` and this
/// fails — `pids.max` is absent because the parent stays undelegated.
#[test]
fn a_parent_left_undelegated_is_repaired_by_the_next_box() {
    let Some(parent) =
        cgroup_parent_or_skip("a_parent_left_undelegated_is_repaired_by_the_next_box")
    else {
        return;
    };

    // Refuse to touch a parent that other boxes are already using: clearing its
    // subtree_control would pull the limits out from under them.
    if let Ok(entries) = std::fs::read_dir(&parent)
        && entries.filter_map(|e| e.ok()).any(|e| e.path().is_dir())
    {
        eprintln!(
            "SKIP a_parent_left_undelegated_is_repaired_by_the_next_box: {} already has box \
                 cgroups; resetting its delegation would disturb them",
            parent.display()
        );
        return;
    }

    // Reproduce the stuck state: parent exists, nothing delegated into it.
    if !parent.exists() {
        std::fs::create_dir(&parent).expect("create boxlite parent cgroup");
    }
    let subtree = parent.join("cgroup.subtree_control");
    let before = std::fs::read_to_string(&subtree).unwrap_or_default();
    for controller in before.split_whitespace() {
        // Disabling is per-controller and may legitimately fail if a child
        // appeared; the precondition assert below is what actually gates the test.
        let _ = std::fs::write(&subtree, format!("-{controller}"));
    }
    let cleared = std::fs::read_to_string(&subtree).unwrap_or_default();
    if !cleared.trim().is_empty() {
        eprintln!(
            "SKIP a_parent_left_undelegated_is_repaired_by_the_next_box: could not clear \
             subtree_control (still {cleared:?})"
        );
        return;
    }

    let home = PerTestBoxHome::new();
    let box_id = start_box(&home);
    let _cleanup = BoxCleanup {
        home: home.path.clone(),
        id: box_id.clone(),
    };

    let pids_max = parent.join(&box_id).join("pids.max");
    let value = std::fs::read_to_string(&pids_max).unwrap_or_else(|e| {
        panic!(
            "{} must exist after starting a box under an undelegated parent: {e}. \
             This is the regression: delegation ran only at parent-creation time, so a \
             parent left behind undelegated was never repaired and every later box ran \
             unconfined.",
            pids_max.display()
        )
    });

    assert_eq!(
        value.trim(),
        "1024",
        "the box started under a stuck parent must still get its limits at {}",
        pids_max.display()
    );
}
