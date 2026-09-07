//! `BoxSockets` — the single authority for a box's Unix socket paths.
//!
//! Unix domain sockets have a `sun_path` limit of 104 bytes (macOS) / 108
//! bytes (Linux), including the NUL terminator. A deep `BOXLITE_HOME` pushes
//! paths like `{home}/boxes/{box_id}/sockets/net.sock-krun.sock` past the
//! limit, and the failure mode is a silent VM-boot hang: libkrun's side of
//! the network socket pair never binds, the net device never comes up, no
//! vCPU starts, and the console stays empty until the guest-ready timeout.
//!
//! Design: every socket is bound and dialed through a short, deterministic
//! symlink — `/tmp/bl-{uid}/{box_id}` → `{box_home}/sockets/` — created when
//! the box directory is prepared and removed with the box. Binding is
//! UNCONDITIONAL (even when the real path would fit) so there is exactly one
//! code path: no length-conditional behavior, no dead zones, and the
//! `sun_path` budget can never silently drift out from under us. The socket
//! files physically live in the real directory; the kernel resolves symlinks
//! during VFS lookup AFTER the `sun_path` length check.
//!
//! Precedent:
//! - Open vSwitch `shorten_name_via_symlink()` (`lib/socket-util-unix.c`) —
//!   the symlink indirection itself.
//! - containerd `SocketAddress()` (`runtime/v2/shim/util_unix.go`) — binding
//!   shim sockets at an unconditionally short address.
//! - tmux `/tmp/tmux-{uid}/` — the 0700 owner-verified parent directory that
//!   prevents other local users from squatting predictable symlink names.
//!
//! Invariant for upgrades: host and shim may know a socket under different
//! *strings* across versions; correctness only requires that both resolve to
//! the same inode (the real file under `sockets/`). Never compare socket
//! paths as strings.
//!
//! Lifecycle: `ensure()` at box prepare and again at guest-connect (the
//! macOS periodic /tmp cleaner can reap symlinks of boxes idle for days —
//! re-ensuring makes that self-healing); `remove()` with the box;
//! [`BoxSockets::sweep_stale`] at runtime startup for crash leftovers.
//!
//! **Library safety**: BoxLite is a library — we must NEVER change the host
//! process's CWD or other process-global state. The symlink approach avoids
//! that. The base is literally `/tmp`, NOT `std::env::temp_dir()`: the host
//! process and the spawned shim must resolve the SAME path, and `TMPDIR` is
//! a per-user env var not necessarily inherited by the shim.

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Maximum allowed socket path length (including the NUL terminator).
/// macOS = 104, Linux = 108. Use the smaller value for cross-platform safety.
const MAX_SUN_PATH: usize = 104;

/// The longest socket path bound inside a box's sockets directory.
///
/// Not a socket we create directly: libkrun derives its side of the network
/// datagram pair by appending `-krun.sock` to the gvproxy socket path
/// (`net.sock`) — see the vendored
/// `libkrun/src/devices/src/virtio/net/unixgram.rs`. With unconditional
/// short binding this is only a sanity check on the SHORT path; a vendor-pin
/// test asserts the derivation still exists upstream.
const LONGEST_SOCKET_NAME: &str = "net.sock-krun.sock";

/// gRPC control socket filename.
const BOX_SOCK: &str = "box.sock";
/// Guest-ready notification socket filename.
const READY_SOCK: &str = "ready.sock";
/// Network backend (gvproxy) socket filename.
const NET_SOCK: &str = "net.sock";

/// Base directory for binding symlinks. Deliberately literal — see module docs.
const SYMLINK_BASE: &str = "/tmp";

/// Per-user parent directory name: `bl-{uid}`.
fn parent_dir_name() -> String {
    // SAFETY: getuid() has no failure modes and touches no memory.
    format!("bl-{}", unsafe { libc::getuid() })
}

/// A box's socket directory: where the sockets physically live (`real_dir`)
/// and the short path everything binds and dials through (`binding_dir`).
///
/// Pure value type — constructible in any process from `(box_id, real_dir)`
/// with no filesystem access, so host and shim always agree on paths.
#[derive(Clone, Debug)]
pub struct BoxSockets {
    box_id: String,
    real_dir: PathBuf,
}

impl BoxSockets {
    pub fn new(box_id: impl Into<String>, real_dir: impl Into<PathBuf>) -> Self {
        Self {
            box_id: box_id.into(),
            real_dir: real_dir.into(),
        }
    }

    /// The real sockets directory (for mkdir, sandbox policy, diagnostics).
    /// Socket files physically live here; do NOT bind/dial this path.
    pub fn real_dir(&self) -> &Path {
        &self.real_dir
    }

    /// The per-user parent of all binding symlinks: `/tmp/bl-{uid}`.
    fn parent_dir() -> PathBuf {
        Path::new(SYMLINK_BASE).join(parent_dir_name())
    }

    /// The short directory every socket is bound and dialed through:
    /// `/tmp/bl-{uid}/{box_id}` (a symlink to [`Self::real_dir`]).
    ///
    /// Pure computation. If the symlink is missing at use time the operation
    /// fails loudly with ENOENT — strictly better than the silent over-length
    /// hang this design exists to prevent; `ensure()` recreates it.
    pub fn binding_dir(&self) -> PathBuf {
        Self::parent_dir().join(&self.box_id)
    }

    /// gRPC control socket (host dials, krun bridges to guest vsock).
    pub fn box_sock(&self) -> PathBuf {
        self.binding_dir().join(BOX_SOCK)
    }

    /// Guest-ready notification socket (host binds the listener).
    pub fn ready_sock(&self) -> PathBuf {
        self.binding_dir().join(READY_SOCK)
    }

    /// Network backend socket (gvproxy binds; libkrun derives a sibling
    /// `net.sock-krun.sock` from this path — the longest path in the dir).
    pub fn net_backend_sock(&self) -> PathBuf {
        self.binding_dir().join(NET_SOCK)
    }

    /// libkrun's side of the macOS Unix datagram network pair.
    ///
    /// This path is derived by vendored libkrun from [`Self::net_backend_sock`]
    /// and is exposed here so path-length checks and sandbox policy share the
    /// same authority for the derived endpoint.
    pub fn net_backend_peer_sock(&self) -> PathBuf {
        self.binding_dir().join(LONGEST_SOCKET_NAME)
    }

    /// Ensure the binding symlink exists and is correct. Idempotent;
    /// tolerates concurrent callers for the same box.
    pub fn ensure(&self) -> BoxliteResult<()> {
        let parent = Self::parent_dir();
        ensure_owned_private_dir(&parent)?;

        // Sanity: even the short path must fit. With a /tmp base and minted
        // box ids this is unreachable; it guards absurd ids/bases loudly.
        let longest = self.binding_dir().join(LONGEST_SOCKET_NAME);
        if longest.as_os_str().len() >= MAX_SUN_PATH {
            return Err(BoxliteError::Internal(format!(
                "Socket path '{}' ({} bytes) exceeds sun_path limit ({} bytes) \
                 even via the binding symlink.",
                longest.display(),
                longest.as_os_str().len(),
                MAX_SUN_PATH,
            )));
        }

        let symlink_path = self.binding_dir();

        // Handle an existing entry at the symlink location.
        match std::fs::symlink_metadata(&symlink_path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&symlink_path).ok().as_deref() == Some(self.real_dir()) {
                    return Ok(()); // already correct
                }
                // Points elsewhere (stale from a previous box) — replace.
                let _ = std::fs::remove_file(&symlink_path);
            }
            Ok(_) => {
                // Regular file or directory — refuse to overwrite.
                return Err(BoxliteError::Internal(format!(
                    "{} exists but is not a symlink — refusing to overwrite",
                    symlink_path.display(),
                )));
            }
            Err(_) => {} // doesn't exist — good
        }

        if let Err(e) = std::os::unix::fs::symlink(self.real_dir(), &symlink_path) {
            // Tolerate a concurrent ensure() for the same box as long as the
            // winner points at our target.
            let racing_winner_ok = e.kind() == std::io::ErrorKind::AlreadyExists
                && std::fs::read_link(&symlink_path).ok().as_deref() == Some(self.real_dir());
            if !racing_winner_ok {
                return Err(BoxliteError::Storage(format!(
                    "Failed to create socket symlink {} → {}: {}",
                    symlink_path.display(),
                    self.real_dir().display(),
                    e,
                )));
            }
        }

        tracing::debug!(
            symlink = %symlink_path.display(),
            target = %self.real_dir().display(),
            "Ensured socket binding symlink"
        );
        Ok(())
    }

    /// Remove the binding symlink. Best-effort, idempotent. The shared
    /// per-user parent directory is left in place.
    pub fn remove(&self) {
        let symlink_path = self.binding_dir();
        if let Err(e) = std::fs::remove_file(&symlink_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %symlink_path.display(),
                error = %e,
                "Failed to remove socket binding symlink"
            );
        }
    }

    /// Paths the sandbox policy needs for socket I/O:
    /// the real dir (writable — socket inodes are created there through the
    /// symlink), the binding symlink (writable — the literal path used at
    /// bind/connect time), and the per-user parent (read-only traversal).
    pub fn policy_paths(&self) -> Vec<(PathBuf, bool)> {
        vec![
            (self.real_dir().to_path_buf(), true),
            (self.binding_dir(), true),
            (Self::parent_dir(), false),
        ]
    }

    /// Remove stale binding symlinks whose targets no longer exist — crash
    /// leftovers the per-box `remove()` never saw (temp-home test runs,
    /// externally deleted homes). Called at runtime startup. Pure hygiene:
    /// `ensure()` already repairs any stale entry per box, so correctness
    /// never depends on this — it only keeps `/tmp/bl-{uid}` inspectable.
    pub fn sweep_stale() {
        sweep_dangling_symlinks_in(&Self::parent_dir());
    }
}

/// Create `dir` as a 0711 directory owned by the current user, verifying an
/// existing entry the way tmux verifies `/tmp/tmux-{uid}`: must be a real
/// directory (not a symlink) owned by us; group/other read/write bits are
/// repaired; anything else is a loud error (a squatted parent is a denial of
/// service, never a redirection).
///
/// 0711 (not 0700) so a box that dropped to its own dedicated UID can still
/// *traverse* this per-runner parent to reach its binding symlink. Listing is
/// denied (no `r`), and each box's sockets stay protected by the ownership of
/// the symlink target (the box's sockets dir), so traverse-only is safe.
fn ensure_owned_private_dir(dir: &Path) -> BoxliteResult<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(0o711).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(BoxliteError::Storage(format!(
                "Failed to create socket symlink dir {}: {}",
                dir.display(),
                e,
            )));
        }
    }

    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| BoxliteError::Storage(format!("Failed to stat {}: {}", dir.display(), e)))?;
    if !meta.file_type().is_dir() {
        return Err(BoxliteError::Internal(format!(
            "{} exists but is not a directory — refusing to use it \
             (possible squatting; remove it or check ownership)",
            dir.display(),
        )));
    }
    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(BoxliteError::Internal(format!(
            "{} is owned by uid {} (expected {}) — refusing to use it \
             (possible squatting; remove it or check ownership)",
            dir.display(),
            meta.uid(),
            uid,
        )));
    }
    // Normalize to exactly 0711: strip any group/other read/write opened up by
    // a foreign umask or an older release, and add the traverse bit that older
    // 0700 dirs lack (so a dropped-UID box can reach its binding symlink).
    if meta.permissions().mode() & 0o777 != 0o711 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o711)).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to tighten permissions on {}: {}",
                dir.display(),
                e,
            ))
        })?;
    }
    Ok(())
}

/// Remove dangling symlinks in `dir`. Only symlinks whose target is gone
/// are touched.
fn sweep_dangling_symlinks_in(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            // Only symlinks (never files/dirs that happen to match) and only
            // when the target no longer exists.
            if meta.file_type().is_symlink() && !path.exists() {
                tracing::debug!(path = %path.display(), "Removing stale socket symlink");
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn unique_sockets(test_tag: &str) -> (tempfile::TempDir, BoxSockets) {
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("sockets");
        std::fs::create_dir_all(&real).unwrap();
        (tmp, BoxSockets::new(test_tag, real))
    }

    // ========================================================================
    // binding paths are pure, short, and per-box
    // ========================================================================

    #[test]
    fn binding_paths_are_short_and_deterministic() {
        let (_tmp, s) = unique_sockets("purepath1");
        let expect = Path::new("/tmp")
            .join(format!("bl-{}", unsafe { libc::getuid() }))
            .join("purepath1");
        assert_eq!(s.binding_dir(), expect);
        assert_eq!(s.box_sock(), expect.join("box.sock"));
        assert_eq!(s.ready_sock(), expect.join("ready.sock"));
        assert_eq!(s.net_backend_sock(), expect.join("net.sock"));
        assert_eq!(s.net_backend_peer_sock(), expect.join("net.sock-krun.sock"));
        for p in [
            s.box_sock(),
            s.ready_sock(),
            s.net_backend_sock(),
            s.net_backend_peer_sock(),
        ] {
            assert!(p.as_os_str().len() < MAX_SUN_PATH);
        }
    }

    #[test]
    fn dead_zone_real_dir_still_binds_short() {
        // Regression guard for the original bug: a real dir where ready.sock
        // fits sun_path but krun's derived net.sock-krun.sock does not. Under
        // unconditional binding the binding paths are short regardless.
        let tmp = tempfile::TempDir::new().unwrap();
        let base_len = tmp.path().as_os_str().len();
        assert!(base_len < 80, "tempdir base unexpectedly long: {base_len}");
        let deep = tmp.path().join("p".repeat(88 - base_len - 1));
        std::fs::create_dir_all(&deep).unwrap();
        let ready = deep.join("ready.sock").as_os_str().len();
        let krun = deep.join(LONGEST_SOCKET_NAME).as_os_str().len();
        assert!(
            ready < MAX_SUN_PATH && krun >= MAX_SUN_PATH,
            "test setup: want dead zone, got ready.sock={ready}, krun={krun}",
        );

        let s = BoxSockets::new("deadzone1", &deep);
        s.ensure().unwrap();
        assert!(
            s.net_backend_peer_sock().as_os_str().len() < MAX_SUN_PATH,
            "binding path must fit with krun's derived suffix"
        );
        assert_eq!(std::fs::read_link(s.binding_dir()).unwrap(), deep);
        s.remove();
    }

    // ========================================================================
    // ensure()
    // ========================================================================

    #[test]
    fn ensure_creates_symlink_and_is_idempotent() {
        let (_tmp, s) = unique_sockets("ens_idem1");
        s.ensure().unwrap();
        let meta = std::fs::symlink_metadata(s.binding_dir()).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(std::fs::read_link(s.binding_dir()).unwrap(), s.real_dir());

        s.ensure().unwrap(); // second call must be a no-op
        assert_eq!(std::fs::read_link(s.binding_dir()).unwrap(), s.real_dir());
        s.remove();
    }

    #[test]
    fn ensure_replaces_stale_symlink() {
        let (_tmp, s) = unique_sockets("ens_stale1");
        std::fs::create_dir_all(BoxSockets::parent_dir()).unwrap();
        let _ = std::fs::remove_file(s.binding_dir());
        symlink(Path::new("/nonexistent/stale/path"), s.binding_dir()).unwrap();

        s.ensure().unwrap();
        assert_eq!(
            std::fs::read_link(s.binding_dir()).unwrap(),
            s.real_dir(),
            "should point at the new target, not the stale one"
        );
        s.remove();
    }

    #[test]
    fn ensure_refuses_to_overwrite_regular_file() {
        let (_tmp, s) = unique_sockets("ens_file1");
        std::fs::create_dir_all(BoxSockets::parent_dir()).unwrap();
        std::fs::write(s.binding_dir(), "not a symlink").unwrap();

        let err = s.ensure().unwrap_err().to_string();
        assert!(err.contains("not a symlink"), "got: {err}");

        let _ = std::fs::remove_file(s.binding_dir());
    }

    #[test]
    fn ensure_refuses_to_overwrite_directory() {
        let (_tmp, s) = unique_sockets("ens_dir1");
        std::fs::create_dir_all(s.binding_dir()).unwrap();

        assert!(s.ensure().is_err());

        let _ = std::fs::remove_dir_all(s.binding_dir());
    }

    #[test]
    fn ensure_repairs_parent_permissions() {
        // A pre-existing 0755 parent (older release / foreign umask) gets its
        // group/other read/write stripped, normalized to 0711 (traverse kept so
        // a dropped-UID box can reach its binding symlink) rather than rejected.
        let parent = BoxSockets::parent_dir();
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (_tmp, s) = unique_sockets("ens_perm1");
        s.ensure().unwrap();

        let mode = std::fs::symlink_metadata(&parent)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o711, "parent must be normalized to 0711");
        s.remove();
    }

    // ========================================================================
    // remove()
    // ========================================================================

    #[test]
    fn remove_deletes_symlink_and_is_idempotent() {
        let (_tmp, s) = unique_sockets("rm_idem1");
        s.ensure().unwrap();
        assert!(std::fs::symlink_metadata(s.binding_dir()).is_ok());

        s.remove();
        assert!(std::fs::symlink_metadata(s.binding_dir()).is_err());
        s.remove(); // second call must not panic
    }

    // ========================================================================
    // sweep_stale()
    // ========================================================================

    #[test]
    fn sweep_removes_dangling_keeps_live() {
        let parent = BoxSockets::parent_dir();
        std::fs::create_dir_all(&parent).unwrap();

        let dead = parent.join("sweep_dead1");
        let _ = std::fs::remove_file(&dead);
        symlink(Path::new("/nonexistent/target/for/test"), &dead).unwrap();

        let (_tmp, live) = unique_sockets("sweep_live1");
        live.ensure().unwrap();

        BoxSockets::sweep_stale();

        assert!(
            std::fs::symlink_metadata(&dead).is_err(),
            "dangling symlink should be removed"
        );
        assert!(
            std::fs::symlink_metadata(live.binding_dir()).is_ok(),
            "live symlink should be kept"
        );
        live.remove();
    }

    #[test]
    fn sweep_ignores_non_symlink_entries() {
        let parent = BoxSockets::parent_dir();
        std::fs::create_dir_all(&parent).unwrap();
        let dir_entry = parent.join("sweep_realdir1");
        let _ = std::fs::remove_dir_all(&dir_entry);
        std::fs::create_dir_all(&dir_entry).unwrap();

        BoxSockets::sweep_stale();

        assert!(
            std::fs::symlink_metadata(&dir_entry).is_ok(),
            "real directories must never be swept"
        );

        let _ = std::fs::remove_dir_all(&dir_entry);
    }

    // ========================================================================
    // Kernel behavior: bind/connect through symlinks (scheme-independent)
    // ========================================================================

    #[test]
    fn bind_and_connect_through_symlink_works() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real_dir = tmp.path().join("real_sockets");
        std::fs::create_dir_all(&real_dir).unwrap();

        let short_link = tmp.path().join("s");
        symlink(&real_dir, &short_link).unwrap();

        let sock_path = short_link.join("test.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        // Socket file physically exists in the real directory.
        assert!(real_dir.join("test.sock").exists());

        let _stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        drop(listener);
    }

    #[test]
    fn bind_through_symlink_with_long_real_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let deep = tmp
            .path()
            .join("very_long_directory_name_that_keeps_going")
            .join("and_another_long_segment_here_too")
            .join("sockets");
        std::fs::create_dir_all(&deep).unwrap();
        assert!(deep.join(LONGEST_SOCKET_NAME).as_os_str().len() >= MAX_SUN_PATH);

        let short_link = tmp.path().join("s");
        symlink(&deep, &short_link).unwrap();
        let short_path = short_link.join("kernel_test.sock");
        assert!(short_path.as_os_str().len() < MAX_SUN_PATH);

        // Core assumption: bind() with the short path succeeds even though
        // the resolved real path exceeds MAX_SUN_PATH.
        let listener = std::os::unix::net::UnixListener::bind(&short_path).unwrap();
        let _stream = std::os::unix::net::UnixStream::connect(&short_path).unwrap();
        assert!(deep.join("kernel_test.sock").exists());
        drop(listener);
    }

    // ========================================================================
    // Vendor pin: LONGEST_SOCKET_NAME tracks libkrun's derivation
    // ========================================================================

    #[test]
    fn vendored_libkrun_still_derives_krun_sock_suffix() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../deps/libkrun-sys/vendor/libkrun/src/devices/src/virtio/net/unixgram.rs"
        );
        let src = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read vendored libkrun source {path}: {e}"));
        assert!(
            src.contains("-krun.sock"),
            "vendored libkrun no longer derives a '-krun.sock' sibling socket; \
             re-derive LONGEST_SOCKET_NAME in net/socket_path.rs from the new \
             vendored behavior before trusting the sun_path sanity check"
        );
    }
}
