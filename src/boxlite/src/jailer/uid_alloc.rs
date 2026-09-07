//! Per-box dedicated host UID/GID allocation.
//!
//! The box's host-side process tree (bwrap monitor + shim + libkrun vCPU and
//! gvproxy threads) drops to a dedicated, per-box UID via `setresuid` in the
//! [`pre_exec`](super::pre_exec) hook before `execve(bwrap)`. This is what makes
//! `RLIMIT_NPROC` correct: the kernel accounts that limit per *real UID*, so
//! charging it against a clean per-box UID bounds the box's own process tree
//! instead of the shared runner UID (which broke box spawn — see
//! [`super::common::rlimit::apply_limits_raw`]). bwrap keeps its identity uid
//! map (no `--uid`), so the dedicated UID maps 1:1 into the new userns.
//!
//! ## Why the claim lives in the box dir
//!
//! A detached box outlives the runner process, so a claim held only by an open
//! `flock` (Nix's `UserLock` model) would be released the moment the runner
//! restarts — and a fresh box could then grab a UID a live box is still using.
//! Instead the *only* durable record of a claim is the `host_uid` file inside
//! the box's own working dir, so the in-use set is always recomputed from the
//! live box dirs — never a separate mutable counter that can desync across a
//! restart. A short `flock` serializes the allocate critical section only.
//! Release is implicit: removing the box dir drops its `host_uid` file.
//!
//! Modeled on Nix's build-user pool and systemd `DynamicUser=`.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// First UID of the pool. Chosen above any real system user and the common
/// `/etc/subuid` delegation range (which typically starts at 100000 and runs to
/// a few million) to avoid colliding with host accounts or rootless container
/// id maps.
pub const POOL_BASE: u32 = 2_000_000;

/// Number of UIDs in the pool — the per-host ceiling on concurrent boxes.
pub const POOL_SIZE: u32 = 60_000;

/// File inside a box dir recording its claimed host UID (decimal text).
const CLAIM_FILE: &str = "host_uid";

/// A box's dedicated host credentials. `gid == uid` by convention: one number
/// per box keeps file ownership and group accounting aligned and the pool
/// trivially collision-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxCredentials {
    pub uid: u32,
    pub gid: u32,
}

/// Allocates and records per-box host UIDs out of a fixed pool.
///
/// Facade: callers use [`allocate`](Self::allocate) (idempotent) and treat the
/// box dir as the durable owner of the claim. There is no `release` — the claim
/// is freed when the box dir is removed.
pub struct UidAllocator {
    boxes_dir: PathBuf,
    lock_path: PathBuf,
    base: u32,
    size: u32,
}

impl UidAllocator {
    /// `boxes_dir` is the parent of every box's working dir (the source of truth
    /// for in-use UIDs); `lock_path` is a file used only to serialize allocation.
    pub fn new(boxes_dir: PathBuf, lock_path: PathBuf) -> Self {
        Self {
            boxes_dir,
            lock_path,
            base: POOL_BASE,
            size: POOL_SIZE,
        }
    }

    /// Whether this process can hand a child a dedicated UID at all. `setresuid`
    /// to an arbitrary UID needs `CAP_SETUID`, which in practice means running
    /// as root. Rootless callers get `None` and must rely on the cgroup
    /// `pids.max` cap alone (no `RLIMIT_NPROC`, no UID drop).
    pub fn is_supported() -> bool {
        // Safe: getuid never fails.
        unsafe { libc::geteuid() == 0 }
    }

    /// Read a box dir's existing claim, if any.
    pub fn recorded(box_dir: &Path) -> Option<u32> {
        fs::read_to_string(box_dir.join(CLAIM_FILE))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
    }

    /// Allocate (or re-read) the dedicated credentials for `box_dir`.
    ///
    /// Idempotent: a box that already recorded a UID (e.g. a stop→start, or a
    /// runner restart re-adopting the box) keeps the same one. Persists the
    /// claim as `<box_dir>/host_uid`. The critical section — scan in-use UIDs,
    /// pick a free one, write the claim — is serialized by an `flock`.
    pub fn allocate(&self, box_dir: &Path) -> BoxliteResult<BoxCredentials> {
        if let Some(uid) = Self::recorded(box_dir) {
            return Ok(BoxCredentials { uid, gid: uid });
        }

        let _guard = PoolLock::acquire(&self.lock_path)?;

        // Re-check under the lock: another caller may have just written it.
        if let Some(uid) = Self::recorded(box_dir) {
            return Ok(BoxCredentials { uid, gid: uid });
        }

        let in_use = self.in_use_uids();
        let uid = (self.base..self.base + self.size)
            .find(|u| !in_use.contains(u))
            .ok_or_else(|| {
                BoxliteError::Internal(format!(
                    "host UID pool exhausted ({}..{})",
                    self.base,
                    self.base + self.size
                ))
            })?;

        // Write the claim before releasing the lock so the next scan sees it.
        let claim = box_dir.join(CLAIM_FILE);
        fs::write(&claim, uid.to_string())
            .map_err(|e| BoxliteError::Internal(format!("write {}: {e}", claim.display())))?;

        Ok(BoxCredentials { uid, gid: uid })
    }

    /// UIDs currently claimed by any box dir. Recomputed every allocation so the
    /// pool stays consistent with reality across runner restarts and crashes.
    fn in_use_uids(&self) -> std::collections::HashSet<u32> {
        let mut set = std::collections::HashSet::new();
        let Ok(entries) = fs::read_dir(&self.boxes_dir) else {
            return set;
        };
        for entry in entries.flatten() {
            if let Some(uid) = Self::recorded(&entry.path()) {
                set.insert(uid);
            }
        }
        set
    }
}

/// An exclusive `flock` held for the allocate critical section only.
struct PoolLock {
    _file: fs::File,
}

impl PoolLock {
    fn acquire(lock_path: &Path) -> BoxliteResult<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|e| {
                BoxliteError::Internal(format!("open uid lock {}: {e}", lock_path.display()))
            })?;
        // Safe: flock on a valid fd. Blocks until the exclusive lock is held;
        // released by close (Drop) — including on crash, by the kernel.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(BoxliteError::Internal(format!(
                "flock uid pool: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Self { _file: file })
    }
}

/// Resolve a local group's GID by name from `/etc/group`.
///
/// The dropped box process keeps a few device groups as supplementary groups so
/// it can still open group-gated host devices (notably `/dev/kvm`, `root:kvm`).
/// `/etc/group` is parsed directly — thread-safe (unlike `getgrnam`) and the
/// relevant groups (`kvm`) are always local. Returns `None` if absent.
pub fn group_gid(name: &str) -> Option<u32> {
    let contents = fs::read_to_string("/etc/group").ok()?;
    for line in contents.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some(name) {
            // group:passwd:GID:members
            return fields.nth(1).and_then(|g| g.trim().parse::<u32>().ok());
        }
    }
    None
}

/// Recursively `lchown` `root` and everything under it to `uid:gid`.
///
/// The box process drops to the dedicated UID, so its private working tree
/// (disks, sockets, the copied shim, logs) must be owned by that UID. Symlinks
/// are chowned without following (`lchown`) so a link inside the box dir can't
/// redirect ownership changes onto a shared file outside it. The box dir holds
/// only per-box files, so this never touches shared bases/images/runtimes.
pub fn chown_tree(root: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::fs as unix_fs;
    unix_fs::lchown(root, Some(uid), Some(gid))?;
    let meta = fs::symlink_metadata(root)?;
    if meta.is_dir() {
        for entry in fs::read_dir(root)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alloc(dir: &Path) -> UidAllocator {
        let boxes = dir.join("boxes");
        fs::create_dir_all(&boxes).unwrap();
        UidAllocator::new(boxes, dir.join("uidpool.lock"))
    }

    fn box_dir(a: &UidAllocator, name: &str) -> PathBuf {
        let d = a.boxes_dir.join(name);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn allocates_distinct_uids_from_pool_base() {
        let tmp = tempfile::tempdir().unwrap();
        let a = alloc(tmp.path());
        let b1 = box_dir(&a, "b1");
        let b2 = box_dir(&a, "b2");

        let c1 = a.allocate(&b1).unwrap();
        let c2 = a.allocate(&b2).unwrap();

        assert_eq!(c1.uid, POOL_BASE);
        assert_eq!(c1.gid, c1.uid);
        assert_eq!(c2.uid, POOL_BASE + 1);
        assert_ne!(c1.uid, c2.uid);
    }

    #[test]
    fn allocate_is_idempotent_per_box() {
        let tmp = tempfile::tempdir().unwrap();
        let a = alloc(tmp.path());
        let b1 = box_dir(&a, "b1");

        let first = a.allocate(&b1).unwrap();
        let again = a.allocate(&b1).unwrap();
        assert_eq!(
            first, again,
            "a box keeps its UID across stop/start/restart"
        );
    }

    #[test]
    fn freed_uid_is_reused_after_box_dir_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let a = alloc(tmp.path());
        let b1 = box_dir(&a, "b1");
        let b2 = box_dir(&a, "b2");

        let c1 = a.allocate(&b1).unwrap();
        let _c2 = a.allocate(&b2).unwrap();
        assert_eq!(c1.uid, POOL_BASE);

        // Box 1 destroyed: its dir (and host_uid claim) is gone.
        fs::remove_dir_all(&b1).unwrap();

        let b3 = box_dir(&a, "b3");
        let c3 = a.allocate(&b3).unwrap();
        assert_eq!(c3.uid, POOL_BASE, "the lowest free UID is reclaimed");
    }

    #[test]
    fn in_use_set_is_rebuilt_from_box_dirs_not_memory() {
        // Simulates a runner restart: a brand-new allocator instance must still
        // see UIDs claimed by existing box dirs.
        let tmp = tempfile::tempdir().unwrap();
        let a1 = alloc(tmp.path());
        let b1 = box_dir(&a1, "b1");
        let c1 = a1.allocate(&b1).unwrap();

        let a2 = UidAllocator::new(a1.boxes_dir.clone(), tmp.path().join("uidpool.lock"));
        let b2 = box_dir(&a2, "b2");
        let c2 = a2.allocate(&b2).unwrap();
        assert_ne!(
            c1.uid, c2.uid,
            "fresh allocator must not reissue a live UID"
        );
    }
}
