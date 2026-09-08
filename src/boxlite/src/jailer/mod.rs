//! Jailer module for BoxLite security isolation.
//!
//! This module provides defense-in-depth security for the boxlite-shim process,
//! implementing multiple isolation layers inspired by Firecracker's jailer.
//!
//! For the complete security design, see [`THREAT_MODEL.md`](./THREAT_MODEL.md).
//!
//! # Architecture
//!
//! ```text
//! Jail (trait — public contract, what callers see)
//! │   prepare()  → pre-spawn setup
//! │   command()  → confined command, ready to spawn
//! │
//! └── Jailer<S: Sandbox> (struct — implements Jail)
//!     │   translates SecurityOptions → SandboxContext
//!     │   delegates to S, adds pre_exec hook
//!     │
//!     └── Sandbox (trait — internal, platform-specific wrapping)
//!         ├── BwrapSandbox       (Linux — bubblewrap)
//!         ├── SeatbeltSandbox    (macOS — sandbox-exec)
//!         └── NoopSandbox        (unsupported / jailer disabled)
//! ```
//!
//! # Security Layers
//!
//! ## Linux
//! 1. **Namespace isolation** - Mount, PID, network namespaces
//! 2. **Chroot/pivot_root** - Filesystem isolation
//! 3. **Seccomp filtering** - Syscall whitelist
//! 4. **Privilege dropping** - Run as unprivileged user
//! 5. **Resource limits** - cgroups v2, rlimits
//!
//! ## macOS
//! 1. **Sandbox (Seatbelt)** - sandbox-exec with SBPL profile
//! 2. **Resource limits** - rlimits
//!
//! # Usage
//!
//! ```ignore
//! let jail = JailerBuilder::new()
//!     .with_box_id(&box_id)
//!     .with_layout(layout)
//!     .with_security(security)
//!     .build()?;
//!
//! jail.prepare()?;
//! let cmd = jail.command(&binary, &args);
//! cmd.spawn()?;
//! ```

// ============================================================================
// Module declarations
// ============================================================================

// Core modules
mod builder;
mod command;
mod common;
mod error;
mod pre_exec;
pub(crate) mod sandbox;
pub(crate) mod shim_copy;

// Linux-only modules
#[cfg(target_os = "linux")]
pub(crate) mod apparmor;
#[cfg(target_os = "linux")]
pub(crate) mod bwrap;
#[cfg(target_os = "linux")]
pub(crate) mod cgroup;
#[cfg(target_os = "linux")]
pub(crate) mod credentials;
#[cfg(target_os = "linux")]
pub mod landlock;
#[cfg(target_os = "linux")]
pub mod seccomp;

// ============================================================================
// Public re-exports
// ============================================================================

// Core types
pub use crate::runtime::advanced_options::{ResourceLimits, SecurityOptions};
pub use builder::JailerBuilder;
pub use error::{ConfigError, IsolationError, JailerError, SystemError};
pub use sandbox::{
    CompositeSandbox, NoopSandbox, PathAccess, PlatformSandbox, Sandbox, SandboxContext,
    UnixSocketAccess,
};

// ============================================================================
// Teardown facade
// ============================================================================

/// Reap any OS processes still belonging to a box's sandbox (best-effort).
///
/// The semantic teardown entry for the isolation layer: callers name the
/// *box*, not the mechanism, so nothing above the jailer has to know how a box
/// is confined. On Linux the box's whole process tree lives in its cgroup, so
/// this reaps it by id; on platforms with no host-side sandbox tree it is a
/// no-op. Idempotent — safe on an already-stopped or never-started box.
#[cfg(target_os = "linux")]
pub(crate) fn reap_box(box_id: &crate::runtime::id::BoxID) -> bool {
    cgroup::kill_cgroup(box_id)
}

/// See the Linux variant. No host-side sandbox process tree to reap here.
#[cfg(not(target_os = "linux"))]
pub(crate) fn reap_box(_box_id: &crate::runtime::id::BoxID) -> bool {
    false
}

// Volume specification (convenience re-export)
pub use crate::runtime::options::VolumeSpec;

// Linux-specific exports
#[cfg(target_os = "linux")]
pub use bwrap::{build_shim_command, is_available as is_bwrap_available};
#[cfg(target_os = "linux")]
pub use landlock::{build_landlock_ruleset, is_landlock_available};
#[cfg(target_os = "linux")]
pub use sandbox::{BwrapSandbox, LandlockSandbox};
#[cfg(target_os = "linux")]
pub use seccomp::SeccompRole;

// macOS-specific exports
#[cfg(target_os = "macos")]
pub use sandbox::SeatbeltSandbox;
#[cfg(target_os = "macos")]
pub use sandbox::seatbelt::{
    SANDBOX_EXEC_PATH, get_base_policy, get_network_policy, is_sandbox_available,
};

// ============================================================================
// Jail trait — public contract
// ============================================================================

use boxlite_shared::errors::BoxliteResult;
use std::path::Path;
use std::process::Command;

/// Process confinement for subprocess isolation.
///
/// Provides the public contract for building isolated commands.
/// Callers don't know or care about the mechanism (bwrap, sandbox-exec, etc.).
///
/// ```ignore
/// let jail: &impl Jail = &jailer;
/// jail.prepare()?;
/// let cmd = jail.command(&binary, &args);
/// cmd.spawn()?;
/// ```
pub trait Jail: Send + Sync {
    /// Pre-spawn setup. Call before `command()`.
    ///
    /// On Linux: userns preflight + cgroup creation.
    /// On macOS: no-op.
    fn prepare(&self) -> BoxliteResult<()>;

    /// Build a confined command, ready to spawn.
    ///
    /// Returns a `Command` with sandbox wrapping and pre_exec hook
    /// (PID file, FD cleanup, rlimits, cgroup join).
    fn command(&self, binary: &Path, args: &[String]) -> Command;
}

// ============================================================================
// Jailer<S: Sandbox> — implements Jail
// ============================================================================

use crate::disk::read_backing_chain;
use crate::runtime::layout::BoxFilesystemLayout;
use crate::volumes::{VolumeShare, classify_volume_share};
use std::path::PathBuf;

// ============================================================================
// Path access rules — granular filesystem permissions
// ============================================================================

/// Build granular [`PathAccess`] rules from the box layout.
///
/// Instead of granting access to the entire box directory, each file and
/// directory is listed individually with the minimum required access level.
///
/// ## Sandbox filesystem layout
///
/// ```text
/// {box_dir}/                          # NOT granted wholesale
/// ├── bin/                        [RO]  # copied shim binary + libkrunfw
/// ├── boot/                       [RO]  # staged custom kernel + initramfs
/// ├── shared/                     [RW]  # guest-visible virtio-fs share root
/// ├── sockets/                    [RW]  # libkrun vsock/unix sockets
/// ├── tmp/                        [RW]  # shim/libkrun transient temp files
/// ├── logs/                       [RW]  # shim logging + VM console output
/// │   ├── boxlite-shim.log                # tracing_appender daily log
/// │   └── console.log                     # libkrun serial console (krun_set_console_output)
/// ├── exit                        [RW]  # crash_capture ExitInfo JSON
/// ├── disks/                      [RW]  # disk images
/// │   ├── disk.qcow2                      # VM/container root disk image
/// │   └── guest-rootfs.qcow2              # guest rootfs COW overlay
/// ├── mounts/                     [--]  # EXCLUDED: host writes, shim reads via shared/
/// ├── shim.pid                    [--]  # EXCLUDED: written by pre_exec (before sandbox)
/// └── shim.stderr                 [--]  # EXCLUDED: host creates before spawn
///
/// External read-only paths:
/// ~/.boxlite/rootfs/              [RO]  # shared guest rootfs backing directory
/// ~/.boxlite/layers/              [RO]  # disk fork points (snapshot/clone bases)
///
/// User volumes:
/// {host_path}                     [per VolumeSpec.read_only]
/// ```
fn build_path_access(layout: &BoxFilesystemLayout, volumes: &[VolumeSpec]) -> Vec<PathAccess> {
    let mut paths = Vec::new();

    // Writable directories (shim creates files inside these at runtime)
    // Note: mounts_dir not included — host writes before spawn, shim accesses via shared_dir
    for dir in [layout.tmp_dir(), layout.logs_dir()] {
        if dir.exists() {
            paths.push(PathAccess {
                path: dir,
                writable: true,
            });
        }
    }

    // Socket paths: the real sockets dir (inodes are created there through
    // the symlink), the /tmp/bl-{uid}/{box_id} binding symlink (the literal
    // path used at bind/connect time), and read-only traversal of the
    // per-user parent. Gated on the REAL dir only (box prepared) — never on
    // global /tmp state, so the emitted profile is deterministic.
    if layout.sockets_dir().exists() {
        for (path, writable) in layout.sockets().policy_paths() {
            paths.push(PathAccess { path, writable });
        }
    }

    // Writable files (pre-created before sandbox for bind-mounting)
    // Note: console_output_path() not listed — lives inside logs/ [RW subpath]
    for file in [
        layout.exit_file_path(),
        layout.disk_path(),
        layout.guest_rootfs_disk_path(),
    ] {
        if file.exists() {
            paths.push(PathAccess {
                path: file,
                writable: true,
            });
        }
    }

    // Qcow2 overlays may reference backing files outside box_dir (for example
    // ~/.boxlite/images/disk-images/*.ext4). Under deny-default seatbelt, those
    // backing files must be explicitly granted as read-only or libkrun fails
    // virtio-blk setup with EINVAL.
    //
    // Cloned boxes have multi-level backing chains (clone → source → base image),
    // so we traverse the full chain to grant access to every backing file.
    for qcow2 in [layout.disk_path(), layout.guest_rootfs_disk_path()] {
        if !qcow2.exists() {
            continue;
        }
        for backing_path in read_backing_chain(&qcow2) {
            if let Some(parent) = backing_path.parent().filter(|p| p.exists()) {
                paths.push(PathAccess {
                    path: parent.to_path_buf(),
                    writable: false,
                });
            }
            paths.push(PathAccess {
                path: backing_path,
                writable: false,
            });
        }
    }

    // Read-only directories (copied shim/libraries and staged boot assets).
    for dir in [layout.bin_dir(), layout.boot_dir()] {
        if dir.exists() {
            paths.push(PathAccess {
                path: dir,
                writable: false,
            });
        }
    }

    // shared/ is exposed as a read-write virtio-fs share root on macOS.
    // libkrun's passthrough fs opens this path during worker init; under
    // deny-default seatbelt it must be writable to avoid EPERM startup panics.
    let shared_dir = layout.shared_dir();
    if shared_dir.exists() {
        paths.push(PathAccess {
            path: shared_dir,
            writable: true,
        });
    }

    // Bases directory: shared backing files (snapshots, clone bases, rootfs cache).
    // The qcow2 overlay references backing files in bases/ directly.
    // Disk images are data (read by the hypervisor, not executed on the host).
    if let Some(bases_dir) = layout
        .root()
        .parent()
        .and_then(|boxes| boxes.parent())
        .map(|home| home.join("bases"))
        .filter(|p| p.exists())
    {
        // The directly-mounted rootfs ext4 is addressed by its *canonical* bases
        // path (BaseDiskManager::new canonicalizes at construction). Grant the
        // canonical path too, or a symlinked BOXLITE_HOME / bases/ leaves bwrap
        // mounting the logical path while libkrun opens the canonical one —
        // "Disk image not found" inside the sandbox.
        let bases_dir = bases_dir.canonicalize().unwrap_or(bases_dir);
        paths.push(PathAccess {
            path: bases_dir,
            writable: false,
        });
    }

    // The in-shim network backend may validate upstream TLS certificates
    // (for example secret-substitution MITM forwarding). Keep host trust
    // stores readable inside the sandbox without granting broader /etc access.
    paths.extend(system_ca_path_access(system_ca_paths()));

    // User volumes. Directories are shared directly, so grant the VMM access.
    // Single files are staged under shared_dir (granted above), so they need no
    // grant here — this also keeps the file's host siblings out of the sandbox.
    // A managed volume names no host path, so there is nothing to grant; the
    // local runtime rejects one before boot anyway (`resolve_user_volumes`).
    for vol in volumes {
        if vol.managed_volume.is_some() {
            continue;
        }
        let p = PathBuf::from(&vol.host_path);
        if let Some(VolumeShare::Dir(dir)) = classify_volume_share(&p) {
            paths.push(PathAccess {
                path: dir,
                writable: !vol.read_only,
            });
        }
    }

    paths
}

fn system_ca_paths() -> [PathBuf; 7] {
    [
        PathBuf::from("/etc/ssl/certs"),
        PathBuf::from("/etc/pki/tls/certs"),
        PathBuf::from("/etc/ca-certificates"),
        PathBuf::from("/etc/ssl/cert.pem"),
        PathBuf::from("/etc/ssl/certs/ca-certificates.crt"),
        PathBuf::from("/etc/pki/tls/certs/ca-bundle.crt"),
        PathBuf::from("/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem"),
    ]
}

fn system_ca_path_access(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathAccess> {
    paths
        .into_iter()
        .filter(|path| path.exists() && !path.is_symlink())
        .map(|path| PathAccess {
            path,
            writable: false,
        })
        .collect()
}

/// Jailer provides process isolation for boxlite-shim.
///
/// Encapsulates security configuration and delegates to a [`Sandbox`]
/// for platform-specific wrapping. All common isolation (FD cleanup,
/// rlimits, cgroup join) is applied via `pre_exec` hook.
///
/// Construct via [`JailerBuilder`]:
///
/// ```ignore
/// use boxlite::jailer::{Jail, JailerBuilder};
///
/// let jail = JailerBuilder::new()
///     .with_box_id(&box_id)
///     .with_layout(layout)
///     .with_security(security)
///     .build()?;
///
/// jail.prepare()?;
/// let cmd = jail.command(&binary, &args);
/// cmd.spawn()?;
/// ```
#[derive(Debug)]
pub struct Jailer<S: Sandbox> {
    /// Platform-specific sandbox implementation.
    sandbox: S,
    /// Security configuration options.
    pub(crate) security: SecurityOptions,
    /// Volume mounts (for sandbox path restrictions).
    pub(crate) volumes: Vec<VolumeSpec>,
    /// Unique box identifier.
    pub(crate) box_id: String,
    /// Box filesystem layout (provides typed path accessors).
    pub(crate) layout: BoxFilesystemLayout,
    /// FDs to preserve through pre_exec: each (source_fd, target_fd) is dup2'd
    /// before FD cleanup. Used for watchdog pipe inheritance across fork.
    pub(crate) preserved_fds: Vec<(std::os::fd::RawFd, i32)>,
    /// Detach-mode process isolation: see [`pre_exec::add_pre_exec_hook`]
    /// — `true` adds `setsid()` to the pre_exec chain, `false` sets the
    /// child's process group to itself at `Command` build time.
    pub(crate) detach: bool,
    /// Caller-defined filesystem permissions required by the confined shim.
    pub(crate) additional_path_access: Vec<PathAccess>,
    /// Whether the shim runs a network backend and needs its AF_UNIX endpoints.
    pub(crate) network_backend_enabled: bool,
}

impl<S: Sandbox> Jail for Jailer<S> {
    fn prepare(&self) -> BoxliteResult<()> {
        if !self.security.jailer_enabled {
            return Ok(());
        }

        // Socket paths are part of the sandbox policy itself. Prepare them
        // before building SandboxContext so failures stop the spawn instead of
        // producing a profile with missing AF_UNIX grants.
        std::fs::create_dir_all(self.layout.sockets_dir()).map_err(|e| {
            boxlite_shared::errors::BoxliteError::Storage(format!(
                "failed to create sockets dir: {e}"
            ))
        })?;
        self.layout.sockets().ensure()?;

        self.sandbox.setup(&self.context())
    }

    fn command(&self, binary: &Path, args: &[String]) -> Command {
        // Pre-create writable files + dirs for sandbox bind-mounting
        if self.security.jailer_enabled {
            let _ = std::fs::create_dir_all(self.layout.logs_dir());
            for path in [
                self.layout.exit_file_path(),
                self.layout.console_output_path(),
            ] {
                if !path.exists() {
                    let _ = std::fs::File::create(&path);
                }
            }
        }

        let mut ctx = self.context();

        // Grant read access to original binary's library directory so the
        // dynamic linker can load libraries from the original location.
        #[allow(clippy::collapsible_if)]
        if self.security.jailer_enabled {
            if let Some(lib_dir) = binary.parent().filter(|d| d.exists()) {
                ctx.paths.push(PathAccess {
                    path: lib_dir.to_path_buf(),
                    writable: false,
                });
            }
        }

        // Shim copy (Firecracker pattern) — shared for both platforms
        let effective_binary = if self.security.jailer_enabled {
            match shim_copy::copy_shim_to_box(binary, self.layout.root()) {
                Ok(copied) => {
                    tracing::info!(
                        original = %binary.display(),
                        copied = %copied.display(),
                        "Using copied shim binary (Firecracker pattern)"
                    );
                    copied
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to copy shim, using original");
                    binary.to_path_buf()
                }
            }
        } else {
            binary.to_path_buf()
        };

        // copy_shim_to_box() created box/bin and the copied shim above, but
        // context() computed the bind list *before* that — so box/bin (which
        // didn't exist yet) was skipped. Add it now, read-only, otherwise bwrap
        // can't see the shim binary it is about to exec (execvp ENOENT).
        #[allow(clippy::collapsible_if)]
        if self.security.jailer_enabled {
            if let Some(bin_dir) = effective_binary.parent().filter(|d| d.exists()) {
                if !ctx.paths.iter().any(|pa| pa.path == bin_dir) {
                    ctx.paths.push(PathAccess {
                        path: bin_dir.to_path_buf(),
                        writable: false,
                    });
                }
            }
        }

        // Start with a bare command. Sandbox.apply() modifies it in-place.
        let mut cmd = Command::new(&effective_binary);
        cmd.args(args);

        if self.security.jailer_enabled && self.sandbox.is_available() {
            tracing::info!(sandbox = self.sandbox.name(), "Applying sandbox isolation");
            self.sandbox.apply(&ctx, &mut cmd);
        } else if self.security.jailer_enabled {
            tracing::warn!("Sandbox not available, falling back to direct command");
        } else {
            tracing::info!("Jailer disabled, running shim without sandbox isolation");
        }

        // Pre-exec hook: PID file, FD preservation, FD cleanup, rlimits. The
        // PID file goes first on purpose — see `pre_exec`'s module docs.
        // Sandbox-specific pre_exec hooks (cgroup, Landlock) are already added
        // by sandbox.apply() above — Command supports multiple pre_exec closures.
        let resource_limits = self.security.resource_limits.clone();
        let pid_writer = self.pid_file_writer();
        pre_exec::add_pre_exec_hook(
            &mut cmd,
            resource_limits,
            pid_writer,
            self.preserved_fds.clone(),
            self.detach,
        );
        cmd
    }
}

impl<S: Sandbox> Jailer<S> {
    /// Get the security options.
    pub fn security(&self) -> &SecurityOptions {
        &self.security
    }

    /// Get mutable reference to security options.
    pub fn security_mut(&mut self) -> &mut SecurityOptions {
        &mut self.security
    }

    /// Get the volumes.
    pub fn volumes(&self) -> &[VolumeSpec] {
        &self.volumes
    }

    /// Get the box ID.
    pub fn box_id(&self) -> &str {
        &self.box_id
    }

    /// Get the box directory.
    pub fn box_dir(&self) -> &Path {
        self.layout.root()
    }

    /// Get the box filesystem layout.
    pub fn layout(&self) -> &BoxFilesystemLayout {
        &self.layout
    }

    /// Get the resource limits.
    pub fn resource_limits(&self) -> &ResourceLimits {
        &self.security.resource_limits
    }

    /// Translate SecurityOptions → SandboxContext.
    ///
    /// Delegates to [`build_path_access`] for granular filesystem rules.
    fn context(&self) -> SandboxContext<'_> {
        let mut paths = build_path_access(&self.layout, &self.volumes);
        paths.extend_from_slice(&self.additional_path_access);
        let unix_sockets = if self.layout.sockets_dir().exists() {
            let sockets = self.layout.sockets();
            let net_backend = sockets.net_backend_sock();
            let net_backend_peer = sockets.net_backend_peer_sock();
            // Seatbelt may audit an AF_UNIX operation against either the short
            // binding-symlink path or the resolved inode path. Grant both
            // aliases for each exact endpoint; never grant the directory.
            let aliases = |binding: PathBuf| {
                let real = binding
                    .file_name()
                    .map(|name| sockets.real_dir().join(name));
                std::iter::once(binding).chain(real)
            };
            let mut bind = vec![sockets.box_sock()];
            let mut connect = vec![sockets.ready_sock()];
            if self.network_backend_enabled {
                bind.extend([
                    net_backend.clone(),
                    net_backend_peer.clone(),
                    crate::net::gvproxy::control_socket_path(&net_backend),
                ]);
                connect.extend([net_backend, net_backend_peer]);
            }
            UnixSocketAccess {
                bind: bind.into_iter().flat_map(&aliases).collect(),
                connect: connect.into_iter().flat_map(aliases).collect(),
            }
        } else {
            UnixSocketAccess::default()
        };
        tracing::debug!(
            box_id = %self.box_id,
            path_count = paths.len(),
            paths = ?paths,
            "Built sandbox path access list"
        );
        if std::env::var_os("BOXLITE_DEBUG_PRINT_SEATBELT").is_some() {
            eprintln!("BOXLITE_DEBUG paths for {}: {:#?}", self.box_id, paths);
        }

        SandboxContext {
            id: &self.box_id,
            paths,
            unix_sockets,
            resource_limits: &self.security.resource_limits,
            network_enabled: self.security.network_enabled,
            sandbox_profile: self.security.sandbox_profile.as_deref(),
            detached: self.detach,
        }
    }

    /// Pre-allocate the PID file writer for the pre_exec hook. Returns
    /// `None` if the path can't be made into a CString (interior NUL).
    fn pid_file_writer(&self) -> Option<crate::util::PidFileWriter> {
        crate::util::PidFileWriter::at(&self.layout.pid_file_path()).ok()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::layout::FsLayoutConfig;
    use tempfile::tempdir;

    fn test_layout(box_dir: PathBuf) -> BoxFilesystemLayout {
        BoxFilesystemLayout::new(box_dir, FsLayoutConfig::without_bind_mount(), false)
    }

    #[test]
    fn test_build_path_access_empty_box_dir() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        let paths = build_path_access(&layout, &[]);

        let existing_ca_paths: Vec<_> = system_ca_paths()
            .into_iter()
            .filter(|p| p.exists() && !p.is_symlink())
            .collect();

        assert_eq!(
            paths.len(),
            existing_ca_paths.len(),
            "empty box dir should only include existing system CA paths"
        );
        for ca_path in existing_ca_paths {
            let entry = paths
                .iter()
                .find(|p| p.path == ca_path)
                .unwrap_or_else(|| panic!("missing CA path {}", ca_path.display()));
            assert!(
                !entry.writable,
                "CA path must be read-only: {}",
                ca_path.display()
            );
        }
    }

    #[test]
    fn test_build_path_access_socket_policy_entries() {
        // Security regression guard: when the box is prepared, the policy
        // must contain all three socket entries — the real sockets dir
        // (writable), the /tmp binding symlink (writable: the literal path
        // used at bind/connect time), and the per-user parent (read-only
        // traversal). Missing any of them breaks sandboxed boots.
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());
        std::fs::create_dir_all(layout.sockets_dir()).unwrap();
        let sockets = layout.sockets();
        sockets.ensure().unwrap();

        let paths = build_path_access(&layout, &[]);

        let find = |p: &std::path::Path| paths.iter().find(|pa| pa.path == p);
        let real = find(&layout.sockets_dir()).expect("real sockets dir entry");
        assert!(real.writable, "real sockets dir must be writable");
        let binding = find(&sockets.binding_dir()).expect("binding symlink entry");
        assert!(binding.writable, "binding symlink must be writable");
        let parent = find(sockets.binding_dir().parent().unwrap()).expect("per-user parent entry");
        assert!(!parent.writable, "per-user parent must be read-only");

        sockets.remove();
    }

    #[test]
    fn test_build_path_access_writable_dirs() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create writable dirs the shim would write to
        // Note: mounts_dir is NOT included — host writes before spawn, shim reads via shared_dir
        std::fs::create_dir_all(layout.sockets_dir()).unwrap();
        std::fs::create_dir_all(layout.tmp_dir()).unwrap();
        std::fs::create_dir_all(layout.logs_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let writable_dirs: Vec<_> = paths
            .iter()
            .filter(|p| p.writable && p.path.is_dir())
            .collect();
        assert_eq!(
            writable_dirs.len(),
            3,
            "Should have 3 writable dirs (sockets, tmp, logs)"
        );

        // All should be writable
        for pa in &writable_dirs {
            assert!(pa.writable);
        }

        let tmp = paths.iter().find(|p| p.path == layout.tmp_dir());
        assert!(tmp.is_some(), "tmp/ should be included");
        assert!(tmp.unwrap().writable, "tmp/ should be writable");
    }

    #[test]
    fn test_build_path_access_writable_files() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Pre-create writable files (as the Jailer::command() does)
        // Note: console_output_path() is inside logs/ [RW subpath], not a standalone file grant
        std::fs::File::create(layout.exit_file_path()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let writable_files: Vec<_> = paths
            .iter()
            .filter(|p| p.writable && p.path.is_file())
            .collect();
        assert_eq!(
            writable_files.len(),
            1,
            "exit only (console.log covered by logs/ subpath)"
        );
    }

    #[test]
    fn test_build_path_access_ro_dirs() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create bin + boot + shared dirs
        std::fs::create_dir_all(layout.bin_dir()).unwrap();
        std::fs::create_dir_all(layout.boot_dir()).unwrap();
        std::fs::create_dir_all(layout.shared_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let bin = paths.iter().find(|p| p.path == layout.bin_dir());
        assert!(bin.is_some(), "bin/ should be included");
        assert!(!bin.unwrap().writable, "bin/ should be read-only");

        let boot = paths.iter().find(|p| p.path == layout.boot_dir());
        assert!(boot.is_some(), "boot/ should be included");
        assert!(!boot.unwrap().writable, "boot/ should be read-only");

        let shared = paths.iter().find(|p| p.path == layout.shared_dir());
        assert!(shared.is_some(), "shared/ should be included");
        assert!(shared.unwrap().writable, "shared/ should be writable");
    }

    #[test]
    fn test_build_path_access_shared_bases_dir() {
        // Simulate the home_dir/boxes/{id} structure
        let dir = tempdir().unwrap();
        let home_dir = dir.path().to_path_buf();
        let boxes_dir = home_dir.join("boxes");
        let box_dir = boxes_dir.join("test-box");
        std::fs::create_dir_all(&box_dir).unwrap();

        // Create home_dir/bases/ (shared backing files)
        let bases_dir = home_dir.join("bases");
        std::fs::create_dir_all(&bases_dir).unwrap();

        let layout = test_layout(box_dir);

        let paths = build_path_access(&layout, &[]);

        // bases/ is granted by its canonical path; compare canonical forms so the
        // assertion holds even when $TMPDIR itself contains a symlink.
        let expected = bases_dir
            .canonicalize()
            .unwrap_or_else(|_| bases_dir.clone());
        let bases_paths: Vec<_> = paths
            .iter()
            .filter(|p| p.path.canonicalize().unwrap_or_else(|_| p.path.clone()) == expected)
            .collect();
        assert_eq!(bases_paths.len(), 1, "Should include home_dir/bases/");
        assert!(!bases_paths[0].writable);
    }

    #[test]
    fn test_build_path_access_canonicalizes_bases_dir_through_symlink() {
        // A symlinked BOXLITE_HOME (or bases/) makes the logical home/bases path
        // diverge from the canonical path the block device opens. The grant must
        // be canonical, or bwrap mounts the logical path while libkrun opens the
        // canonical one and fails with "Disk image not found".
        let real_home = tempdir().unwrap();
        let real_home = real_home.path().to_path_buf();
        let link_root = tempdir().unwrap();
        let link_home = link_root.path().join("boxlite-home-link");
        std::os::unix::fs::symlink(&real_home, &link_home).unwrap();

        // box_dir lives under the symlinked home.
        let box_dir = link_home.join("boxes").join("test-box");
        std::fs::create_dir_all(&box_dir).unwrap();

        // bases/ under the real (canonical) home.
        let real_bases = real_home.join("bases");
        std::fs::create_dir_all(&real_bases).unwrap();

        let layout = test_layout(box_dir);
        let paths = build_path_access(&layout, &[]);

        let expected = real_bases.canonicalize().unwrap();
        assert!(
            paths.iter().any(|p| p.path == expected && !p.writable),
            "bases/ must be granted by its canonical path ({}), not the logical symlink path",
            expected.display()
        );
    }

    #[test]
    fn test_build_path_access_includes_system_ca_paths_readonly() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());
        let existing_ca_paths: Vec<_> = system_ca_paths()
            .into_iter()
            .filter(|p| p.exists() && !p.is_symlink())
            .collect();

        if existing_ca_paths.is_empty() {
            return;
        }

        let paths = build_path_access(&layout, &[]);

        for ca_path in existing_ca_paths {
            let entry = paths
                .iter()
                .find(|p| p.path == ca_path)
                .unwrap_or_else(|| panic!("missing CA path {}", ca_path.display()));
            assert!(
                !entry.writable,
                "CA path must be read-only: {}",
                ca_path.display()
            );
        }
    }

    #[test]
    fn test_system_ca_path_access_skips_symlinked_ca_files() {
        let dir = tempdir().unwrap();
        let regular = dir.path().join("ca-certificates.crt");
        let target = dir.path().join("tls-ca-bundle.pem");
        let symlink = dir.path().join("ca-bundle.crt");
        std::fs::write(&regular, "regular CA").unwrap();
        std::fs::write(&target, "symlink target CA").unwrap();
        std::os::unix::fs::symlink(&target, &symlink).unwrap();

        let paths = system_ca_path_access([regular.clone(), symlink.clone()]);

        assert!(
            paths
                .iter()
                .any(|entry| entry.path == regular && !entry.writable),
            "regular CA files must remain readable"
        );
        assert!(
            paths.iter().all(|entry| entry.path != symlink),
            "symlinked CA files must not become bwrap bind destinations"
        );
    }

    #[test]
    fn test_build_path_access_includes_qcow2_backing_file() {
        use crate::disk::{BackingFormat, Qcow2Helper};

        let dir = tempdir().unwrap();
        let home_dir = dir.path().to_path_buf();
        let boxes_dir = home_dir.join("boxes");
        let box_dir = boxes_dir.join("test-box");
        std::fs::create_dir_all(&box_dir).unwrap();

        // Simulate image cache backing file outside box_dir.
        let disk_images_dir = home_dir.join("images").join("disk-images");
        std::fs::create_dir_all(&disk_images_dir).unwrap();
        let base_disk = disk_images_dir.join("sha256-test.ext4");
        std::fs::write(&base_disk, vec![0u8; 1024 * 1024]).unwrap();

        let layout = test_layout(box_dir);
        let child_disk = Qcow2Helper::create_cow_child_disk(
            &base_disk,
            BackingFormat::Raw,
            &layout.disk_path(),
            16 * 1024 * 1024,
        )
        .unwrap();

        let paths = build_path_access(&layout, &[]);

        let expected_backing = base_disk.canonicalize().unwrap_or(base_disk);
        let backing_paths: Vec<_> = paths
            .iter()
            .filter(|p| {
                p.path.canonicalize().unwrap_or_else(|_| p.path.clone()) == expected_backing
            })
            .collect();
        assert_eq!(
            backing_paths.len(),
            1,
            "Expected qcow2 backing file to be included in sandbox paths"
        );
        assert!(!backing_paths[0].writable, "Backing file must be read-only");

        // Keep child disk alive until after assertions.
        let _ = child_disk.path();
    }

    #[test]
    fn test_build_path_access_volumes() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir);

        // Create volume host paths
        let vol_ro = dir.path().join("input");
        let vol_rw = dir.path().join("output");
        std::fs::create_dir_all(&vol_ro).unwrap();
        std::fs::create_dir_all(&vol_rw).unwrap();

        let volumes = vec![
            VolumeSpec {
                managed_volume: None,
                host_path: vol_ro.to_string_lossy().to_string(),
                guest_path: "/mnt/input".to_string(),
                read_only: true,
            },
            VolumeSpec {
                managed_volume: None,
                host_path: vol_rw.to_string_lossy().to_string(),
                guest_path: "/mnt/output".to_string(),
                read_only: false,
            },
        ];

        let paths = build_path_access(&layout, &volumes);

        let vol_paths: Vec<_> = paths
            .iter()
            .filter(|p| p.path == vol_ro || p.path == vol_rw)
            .collect();
        assert_eq!(vol_paths.len(), 2, "Both volumes should be listed");

        let ro_vol = vol_paths.iter().find(|p| p.path == vol_ro).unwrap();
        assert!(!ro_vol.writable, "RO volume should be read-only");

        let rw_vol = vol_paths.iter().find(|p| p.path == vol_rw).unwrap();
        assert!(rw_vol.writable, "RW volume should be writable");
    }

    #[test]
    fn test_build_path_access_nonexistent_volume_skipped() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        let volumes = vec![VolumeSpec {
            managed_volume: None,
            host_path: "/does/not/exist".to_string(),
            guest_path: "/mnt/data".to_string(),
            read_only: true,
        }];

        let paths = build_path_access(&layout, &volumes);

        assert!(
            paths.iter().all(|p| p.path != Path::new("/does/not/exist")),
            "Nonexistent volume should be skipped"
        );
    }

    #[test]
    fn test_build_path_access_single_file_grants_no_host_dir() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        let parent = dir.path().join("cfg");
        std::fs::create_dir_all(&parent).unwrap();
        let file = parent.join("app.conf");
        std::fs::write(&file, "k=v\n").unwrap();

        let volumes = vec![VolumeSpec {
            managed_volume: None,
            host_path: file.to_string_lossy().to_string(),
            guest_path: "/etc/app.conf".to_string(),
            read_only: true,
        }];

        let paths = build_path_access(&layout, &volumes);

        // A single file is staged under shared_dir, so it must not widen path
        // access to the file or its parent (which would expose host siblings).
        assert!(
            paths.iter().all(|p| p.path != file && p.path != parent),
            "single-file volume must not grant its host file or parent dir"
        );
    }

    #[test]
    fn test_build_path_access_no_whole_box_dir() {
        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create all subdirectories
        std::fs::create_dir_all(layout.sockets_dir()).unwrap();
        std::fs::create_dir_all(layout.mounts_dir()).unwrap();
        std::fs::create_dir_all(layout.logs_dir()).unwrap();
        std::fs::create_dir_all(layout.bin_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        // The box_dir itself should NOT appear as a path — only its children
        assert!(
            paths.iter().all(|p| p.path != box_dir),
            "box_dir should not be listed wholesale — only granular paths"
        );
    }

    /// mounts_dir must NOT appear in path access even when it exists on disk.
    /// The shim never writes to mounts/ — host writes before spawn, shim reads via shared_dir.
    #[test]
    fn test_build_path_access_mounts_dir_excluded() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());
        let mounts_base = layout.shared_layout().base().to_path_buf();

        // Create mounts_dir AND other dirs that SHOULD appear
        std::fs::create_dir_all(&mounts_base).unwrap();
        std::fs::create_dir_all(layout.sockets_dir()).unwrap();
        std::fs::create_dir_all(layout.logs_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        // mounts_dir must be absent
        assert!(
            paths.iter().all(|p| p.path != mounts_base),
            "mounts_dir must NOT appear in path access"
        );

        // sockets_dir should be present (sanity check)
        assert!(
            paths.iter().any(|p| p.path == layout.sockets_dir()),
            "sockets_dir should be present"
        );
    }

    /// shared_dir must be writable because it is exposed as an RW virtio-fs share root.
    #[test]
    fn test_build_path_access_shared_dir_is_writable() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        std::fs::create_dir_all(layout.shared_dir()).unwrap();

        let paths = build_path_access(&layout, &[]);

        let shared = paths.iter().find(|p| p.path == layout.shared_dir());
        assert!(shared.is_some(), "shared_dir should be in path access");
        assert!(shared.unwrap().writable, "shared_dir must be writable");
    }

    /// After pre-creating files (as Jailer::command() does), all appear in path access as writable.
    /// console.log lives inside logs/ [RW subpath] — no separate PathAccess entry needed.
    #[test]
    fn test_build_path_access_captures_all_precreated_files() {
        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());

        // Simulate pre-create (same as Jailer::command())
        std::fs::create_dir_all(layout.logs_dir()).unwrap();
        std::fs::File::create(layout.exit_file_path()).unwrap();
        std::fs::File::create(layout.console_output_path()).unwrap();

        let paths = build_path_access(&layout, &[]);

        // logs_dir covers both shim logs and console.log
        let logs = paths.iter().find(|p| p.path == layout.logs_dir());
        assert!(logs.is_some(), "logs_dir should be in path access");
        assert!(logs.unwrap().writable, "logs_dir should be writable");

        let exit = paths.iter().find(|p| p.path == layout.exit_file_path());
        assert!(exit.is_some(), "exit_file should be in path access");
        assert!(exit.unwrap().writable, "exit_file should be writable");

        // console.log should NOT have its own PathAccess — covered by logs/ subpath
        let console = paths
            .iter()
            .find(|p| p.path == layout.console_output_path());
        assert!(
            console.is_none(),
            "console.log should not be a standalone path access (covered by logs/)"
        );
    }

    /// End-to-end: builder -> prepare -> command with real tempdir.
    /// Verifies all the pieces (builder, layout, path access, pre-create) work together.
    #[test]
    fn test_jailer_full_flow_with_real_tempdir() {
        use crate::jailer::builder::JailerBuilder;
        use crate::runtime::advanced_options::SecurityOptions;

        let dir = tempdir().unwrap();
        let box_dir = dir.path().to_path_buf();
        let layout = test_layout(box_dir.clone());

        // Create a volume dir
        let vol_dir = dir.path().join("my-volume");
        std::fs::create_dir_all(&vol_dir).unwrap();

        let security = SecurityOptions {
            jailer_enabled: true,
            ..SecurityOptions::default()
        };

        let jail = JailerBuilder::new()
            .with_box_id("e2e-test")
            .with_layout(layout.clone())
            .with_security(security)
            .with_network_backend_enabled(true)
            .with_volumes(vec![VolumeSpec {
                managed_volume: None,
                host_path: vol_dir.to_string_lossy().to_string(),
                guest_path: "/mnt/data".to_string(),
                read_only: false,
            }])
            .build()
            .unwrap();

        // prepare() should succeed
        jail.prepare().unwrap();

        // AF_UNIX permissions are exact endpoints, not socket directories.
        // Each endpoint has a short binding path and a resolved inode path.
        let ctx = jail.context();
        let socket_names = |paths: &[PathBuf]| {
            let mut names = paths
                .iter()
                .map(|path| {
                    path.file_name()
                        .expect("socket endpoint must have a filename")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(
            socket_names(&ctx.unix_sockets.bind),
            [
                "box.sock",
                "box.sock",
                "gvproxy-ctl.sock",
                "gvproxy-ctl.sock",
                "net.sock",
                "net.sock",
                "net.sock-krun.sock",
                "net.sock-krun.sock",
            ]
        );
        assert_eq!(
            socket_names(&ctx.unix_sockets.connect),
            [
                "net.sock",
                "net.sock",
                "net.sock-krun.sock",
                "net.sock-krun.sock",
                "ready.sock",
                "ready.sock",
            ]
        );

        // command() should not panic and should pre-create files
        let _cmd = jail.command(
            std::path::Path::new("/usr/bin/boxlite-shim"),
            &["--engine".to_string(), "Libkrun".to_string()],
        );

        // Verify pre-create side effects
        assert!(
            layout.logs_dir().exists(),
            "logs_dir should be created by command()"
        );
        assert!(
            layout.sockets_dir().exists(),
            "sockets_dir should be created by prepare() before policy generation"
        );
        let binding_meta = std::fs::symlink_metadata(layout.sockets().binding_dir())
            .expect("socket binding path should exist after prepare()");
        assert!(
            binding_meta.file_type().is_symlink(),
            "socket binding path should be a symlink after prepare()"
        );
        assert_eq!(
            std::fs::read_link(layout.sockets().binding_dir())
                .expect("socket binding symlink should be readable"),
            layout.sockets_dir(),
            "socket binding symlink should target sockets_dir()"
        );
        assert!(
            layout.exit_file_path().exists(),
            "exit file should be created by command()"
        );
        assert!(
            layout.console_output_path().exists(),
            "console.log should be created by command()"
        );
    }

    #[test]
    fn no_network_backend_grants_only_control_plane_sockets() {
        use crate::jailer::builder::JailerBuilder;

        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());
        let jail = JailerBuilder::new()
            .with_box_id("offline-box")
            .with_layout(layout)
            .build()
            .unwrap();

        jail.prepare().unwrap();
        let ctx = jail.context();
        let socket_names = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|path| {
                    path.file_name()
                        .expect("socket endpoint must have a filename")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            socket_names(&ctx.unix_sockets.bind),
            ["box.sock", "box.sock"]
        );
        assert_eq!(
            socket_names(&ctx.unix_sockets.connect),
            ["ready.sock", "ready.sock"]
        );
    }

    #[test]
    fn test_prepare_reports_socket_setup_failures() {
        use crate::jailer::builder::JailerBuilder;
        use crate::runtime::advanced_options::SecurityOptions;

        let dir = tempdir().unwrap();
        let layout = test_layout(dir.path().to_path_buf());
        std::fs::write(layout.sockets_dir(), b"not a directory").unwrap();

        let jail = JailerBuilder::new()
            .with_box_id("bad-sockets")
            .with_layout(layout)
            .with_security(SecurityOptions {
                jailer_enabled: true,
                ..SecurityOptions::default()
            })
            .build()
            .unwrap();

        let error = jail.prepare().unwrap_err().to_string();
        assert!(
            error.contains("failed to create sockets dir") || error.contains("Not a directory"),
            "socket setup failure should be reported before command construction: {error}"
        );
    }
}
