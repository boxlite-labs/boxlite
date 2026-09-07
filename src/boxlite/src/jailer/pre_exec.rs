//! Pre-execution hook for process isolation.
//!
//! This module provides the pre-execution hook that runs after `fork()` but
//! before the new program starts in the child process.
//!
//! # What it does
//!
//! 1. **Write PID file** - Single source of truth for process tracking
//! 2. **Close inherited FDs** - Prevents information leakage
//! 3. **Apply rlimits** - Resource limits (max files, memory, CPU time, etc.)
//! 4. **Drop to the box's dedicated UID/GID** - privileged callers only; done
//!    after rlimits so `RLIMIT_NPROC` is charged against the dedicated UID
//!
//! The order is load-bearing. `Command::spawn` blocks the parent only until
//! the child closes its CLOEXEC exec-error pipe, and step 2 is what closes
//! it — so anything after that point races the parent. The PID file goes
//! first to keep "spawn returned" meaning "shim.pid is complete".
//!
//! Sandbox-specific pre_exec hooks (cgroup join, Landlock restriction) are
//! added by each sandbox's `apply()` method — they run before this hook
//! since `Command::pre_exec` closures execute in registration order.
//!
//! # Safety
//!
//! The hook runs in a very restricted context:
//! - Only async-signal-safe syscalls are allowed
//! - No memory allocation (no Box, Vec, String)
//! - No mutex operations
//! - No logging (tracing, println)
//!
//! See the [`common`](crate::jailer::common) module for async-signal-safe utilities.

use crate::jailer::common;
use crate::runtime::advanced_options::ResourceLimits;
use crate::util::{PidFileWriter, ShimPidRecord};
use std::os::fd::RawFd;
use std::process::Command;

/// Add pre-execution hook for process isolation (async-signal-safe).
///
/// Runs after fork() but before the new program starts in the child process.
/// Applies, in order: PID file writing, FD preservation (dup2), FD cleanup,
/// rlimits, dedicated-UID drop. See the module docs for why the PID file
/// goes first.
///
/// # Arguments
///
/// * `cmd` - The Command to add the hook to
/// * `resource_limits` - Resource limits to apply
/// * `pid_writer` - Async-signal-safe writer (pre-allocated in the parent)
/// * `preserved_fds` - FDs to preserve: each `(source, target)` is dup2'd before cleanup.
///   After dup2, all FDs above the highest target are closed.
///   Pass empty vec for default behavior (close all FDs >= 3).
///
/// # Safety
///
/// This function uses `unsafe` to set the hook. The hook itself
/// only uses async-signal-safe operations:
/// - `dup2()` / `close()` / `close_range()` syscalls
/// - `setrlimit()` syscall
/// - `open()` / `write()` / `close()` syscalls (for PID file)
/// - `getpid()` syscall
///
/// **Do NOT add any of the following to the hook:**
/// - Logging (tracing, println, eprintln)
/// - Memory allocation (Box, Vec, String creation)
/// - Mutex operations
/// - Most Rust standard library functions
pub fn add_pre_exec_hook(
    cmd: &mut Command,
    resource_limits: ResourceLimits,
    pid_writer: Option<PidFileWriter>,
    preserved_fds: Vec<(RawFd, i32)>,
    detach: bool,
    drop_to: Option<(u32, u32, Vec<u32>)>,
) {
    use std::os::unix::process::CommandExt;

    // Detach=false → child's own process group at Command-build time
    // so a later `killpg(shim_pid, SIGKILL)` reaps the shim plus its
    // grandchildren (libkrun threads, gvproxy) atomically.
    //
    // Gated on `!detach` because the detached branch below uses
    // `setsid()`, which creates a new session AND a new pgroup with
    // the child as leader of both. Calling `process_group(0)` here
    // would make the child a pgroup leader before `setsid()` runs;
    // `setsid()` then fails with EPERM (POSIX: setsid is forbidden
    // for an existing pgroup leader). The branches are exclusive on
    // purpose — `setsid()` already covers the pgroup case.
    if !detach {
        cmd.process_group(0);
    }

    // SAFETY: The hook only uses async-signal-safe syscalls.
    // See module documentation for details.
    unsafe {
        cmd.pre_exec(move || {
            // 1. Write PID file.
            //
            // This must precede the FD cleanup below. `Command::spawn` keeps
            // the parent blocked only until the child closes its CLOEXEC
            // exec-error pipe, and closing inherited FDs is what closes it —
            // so every later step races the parent. Publishing the identity
            // first makes "spawn returned" mean "shim.pid is complete", which
            // is the ordering the init pipeline reads it under.
            //
            // Safe to run first: the write opens, writes and closes its own
            // descriptor, so it neither survives into the cleanup below nor
            // collides with a preserved-FD target.
            if let Some(ref writer) = pid_writer {
                writer
                    .write_shim(&ShimPidRecord::current())
                    .map_err(std::io::Error::from_raw_os_error)?;
            }

            // 2. FD preservation + cleanup
            // If preserved_fds is non-empty, dup2 each (source -> target),
            // then close everything above the highest target.
            // Otherwise, close all FDs >= 3 (default behavior).
            if !preserved_fds.is_empty() {
                for &(source, target) in &preserved_fds {
                    if source != target {
                        libc::dup2(source, target);
                    }
                }
                let first_close = preserved_fds.iter().map(|(_, t)| *t).max().unwrap() + 1;
                common::fd::close_fds_from(first_close)
                    .map_err(std::io::Error::from_raw_os_error)?;
            } else {
                common::fd::close_inherited_fds_raw().map_err(std::io::Error::from_raw_os_error)?;
            }

            // 3. Apply resource limits (rlimits)
            common::rlimit::apply_limits_raw(&resource_limits)
                .map_err(std::io::Error::from_raw_os_error)?;

            // 4. Drop to the box's dedicated UID/GID (privileged callers only).
            // Done after rlimits so RLIMIT_NPROC — set above while still the
            // spawning UID — is enforced at bwrap's `clone(CLONE_NEWUSER)`
            // against this clean per-box UID, not the shared runner UID. Order
            // mirrors Firecracker's jailer: gid, drop supplementary groups,
            // then uid (so CAP_SETGID/CAP_SETUID are dropped last). All three
            // are async-signal-safe.
            #[cfg(target_os = "linux")]
            if let Some((uid, gid, ref groups)) = drop_to {
                if libc::setresgid(gid, gid, gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // Set the supplementary groups to exactly `groups` (device
                // groups like kvm, so the dropped UID can still open /dev/kvm);
                // empty list clears all. `as_ptr()` on an empty Vec is unused
                // because the count is 0.
                let (n, ptr) = (groups.len() as libc::size_t, groups.as_ptr());
                if libc::setgroups(n, ptr) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setresuid(uid, uid, uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            #[cfg(not(target_os = "linux"))]
            let _ = &drop_to;

            // 5. Detach=true → setsid: child becomes a session leader,
            // detaching from the parent's controlling terminal. Without
            // this a SIGHUP on the parent's terminal cascades into the
            // daemon (the `BoxOptions::detach` contract relies on it).
            // `setsid` is async-signal-safe.
            if detach && libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_hook_compiles() {
        let mut cmd = Command::new("/bin/echo");
        let limits = ResourceLimits::default();

        add_pre_exec_hook(&mut cmd, limits, None, vec![], false, None);
    }

    #[test]
    fn test_add_hook_with_pid_file() {
        let mut cmd = Command::new("/bin/echo");
        let limits = ResourceLimits::default();
        let writer = PidFileWriter::at(std::path::Path::new("/tmp/test.pid")).ok();
        add_pre_exec_hook(&mut cmd, limits, writer, vec![], false, None);
    }

    #[test]
    fn pre_exec_pid_record_declares_runtime_port_control() {
        use std::os::fd::AsRawFd;

        let dir = tempfile::tempdir().expect("create temp directory");
        let pid_path = dir.path().join("shim.pid");
        let writer = PidFileWriter::at(&pid_path).expect("create PID writer");
        let keepalive = std::fs::File::open("/dev/null").expect("open preserved descriptor");
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "true"]);

        add_pre_exec_hook(
            &mut cmd,
            ResourceLimits::default(),
            Some(writer),
            // Keep the test harness's private Command error pipe below the
            // cleanup boundary. Production supplies its own preserved transport
            // descriptors; this stand-in otherwise has none.
            vec![(keepalive.as_raw_fd(), 1023)],
            false,
            None,
        );
        let status = cmd.status().expect("spawn child with pre-exec hook");
        assert!(status.success());

        let contents = std::fs::read_to_string(pid_path).expect("read shim PID record");
        assert_eq!(
            contents.lines().nth(2),
            Some("services-mux-v1"),
            "the spawn boundary must identify ServicesMux ownership before the parent can recover \
             the live shim"
        );
    }

    /// Regression: `spawn()` returning must imply `shim.pid` is readable as a
    /// complete record.
    ///
    /// The parent is released when the child closes `Command`'s CLOEXEC
    /// exec-error pipe, and the hook's FD cleanup is what closes it. Anything
    /// the hook does after that point races every consumer of the file — the
    /// init pipeline reads it as soon as `VmmSpawn` returns.
    ///
    /// Both production hook shapes are covered: the default spawn preserves the
    /// watchdog pipe onto FD 3 and closes from 4 up, while a detached spawn
    /// preserves nothing and closes everything from 3 up.
    #[test]
    fn pid_record_is_complete_before_spawn_returns() {
        use std::os::fd::AsRawFd;

        /// One of the two hook configurations production actually spawns with.
        struct HookShape {
            name: &'static str,
            preserved_fds: Vec<(RawFd, i32)>,
            detach: bool,
        }

        let keepalive = std::fs::File::open("/dev/null").expect("stand in for the watchdog pipe");
        let shapes = [
            HookShape {
                name: "default",
                preserved_fds: vec![(keepalive.as_raw_fd(), 3)],
                detach: false,
            },
            HookShape {
                name: "detached",
                preserved_fds: Vec::new(),
                detach: true,
            },
        ];

        for HookShape {
            name: shape,
            preserved_fds,
            detach,
        } in shapes
        {
            for attempt in 0..32 {
                let dir = tempfile::tempdir().expect("create temp directory");
                let pid_path = dir.path().join("shim.pid");
                let writer = PidFileWriter::at(&pid_path).expect("create PID writer");
                let mut cmd = Command::new("/bin/sh");
                cmd.args(["-c", "exec sleep 5"]);
                add_pre_exec_hook(
                    &mut cmd,
                    ResourceLimits::default(),
                    Some(writer),
                    preserved_fds.clone(),
                    detach,
                    None,
                );

                let mut child = cmd.spawn().expect("spawn child with pre-exec hook");
                // Read before reaping, through the same decoder production uses:
                // this is the exact instant VmmSpawn hands the box on.
                let observed = crate::util::PidFileReader::at(&pid_path).read_shim();
                let _ = child.kill();
                let _ = child.wait();

                let record = observed.unwrap_or_else(|error| {
                    panic!("{shape} attempt {attempt}: shim.pid unreadable when spawn() returned: {error}")
                });
                assert_eq!(record.identity().pid, child.id());
                assert!(
                    record.has_runtime_port_control(),
                    "{shape} attempt {attempt}: record was truncated before its \
                     capability line, which production reads as a legacy shim"
                );
            }
        }
    }

    #[test]
    fn test_add_hook_with_preserved_fds() {
        let mut cmd = Command::new("/bin/echo");
        let limits = ResourceLimits::default();

        // Simulate preserving fd 5 → target fd 3
        add_pre_exec_hook(&mut cmd, limits, None, vec![(5, 3)], false, None);
    }

    /// Verifies, via the child's own `/proc` files, that `drop_to` lands the
    /// child on the dedicated UID and that `RLIMIT_NPROC` is in force there —
    /// i.e. the per-box process cap is charged against the dedicated UID, not
    /// the spawning UID. Needs `CAP_SETUID` (root), so it self-skips otherwise.
    #[cfg(target_os = "linux")]
    #[test]
    fn drop_to_sets_child_uid_and_scopes_nproc() {
        if unsafe { libc::geteuid() } != 0 {
            eprintln!(
                "skipping drop_to_sets_child_uid_and_scopes_nproc: requires root (CAP_SETUID)"
            );
            return;
        }
        use std::process::Stdio;
        let uid = 2_000_123u32;
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("grep '^Uid:' /proc/self/status; grep 'Max processes' /proc/self/limits")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let limits = ResourceLimits {
            max_processes: Some(100),
            ..Default::default()
        };
        add_pre_exec_hook(
            &mut cmd,
            limits,
            None,
            vec![],
            false,
            Some((uid, uid, vec![])),
        );

        let out = cmd.output().expect("spawn /bin/sh");
        let report = String::from_utf8_lossy(&out.stdout);
        eprintln!("--- child /proc evidence ---\n{report}---");

        // Real/effective/saved UID are all the dedicated UID.
        assert!(
            report.contains(&format!("Uid:\t{uid}\t{uid}\t{uid}")),
            "child should run as dedicated UID {uid}; got:\n{report}"
        );
        // RLIMIT_NPROC soft cap is 100, accounted against UID {uid}.
        assert!(
            report
                .lines()
                .any(|l| l.starts_with("Max processes") && l.contains("100")),
            "child RLIMIT_NPROC should be 100; got:\n{report}"
        );
    }
}
