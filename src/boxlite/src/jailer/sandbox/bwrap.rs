//! BwrapSandbox — Linux isolation via bubblewrap.
//!
//! Implements the [`Sandbox`] trait using bubblewrap (bwrap) for
//! namespace isolation, bind mounts, and environment sanitization.

use super::{Sandbox, SandboxContext};
use crate::jailer::bwrap;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::process::Command;

/// Linux sandbox using bubblewrap for namespace isolation.
#[derive(Debug)]
pub struct BwrapSandbox;

impl BwrapSandbox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for BwrapSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Sandbox for BwrapSandbox {
    fn is_available(&self) -> bool {
        bwrap::is_available()
    }

    fn setup(&self, _ctx: &SandboxContext) -> BoxliteResult<()> {
        // Preflight: verify bwrap can create user namespaces before proceeding.
        if bwrap::is_available()
            && let Err(diagnostic) = bwrap::can_create_user_namespace()
        {
            // The fix commands and the `SecurityOptions::disabled()` escape hatch
            // need a host shell — operator-only. The tenant gets one sentence.
            tracing::error!(
                "Sandbox preflight failed: bwrap cannot create user namespaces.\n{}\n\
                 To skip the sandbox (development only): SecurityOptions::disabled()",
                diagnostic.operator()
            );
            return Err(BoxliteError::Config(diagnostic.into_client()));
        }

        // Host cgroup setup is handled by the Jailer (see Jailer::prepare),
        // decoupled from the bwrap user-namespace path so resource limits
        // apply even when process-isolation sandboxing is disabled.
        Ok(())
    }

    fn apply(&self, ctx: &SandboxContext, cmd: &mut Command) {
        let binary = cmd.get_program().to_owned();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let mut bwrap_cmd = bwrap::BwrapCommand::new();

        // =====================================================================
        // Namespace and session isolation
        // =====================================================================
        bwrap_cmd.with_default_namespaces();
        // A detached box (`run -d`) must outlive the launching process: bwrap's
        // --die-with-parent (PR_SET_PDEATHSIG) would otherwise kill the shim/VM
        // the instant the launcher returns, so the box is born Stopped. Only
        // foreground boxes — which should die with their launcher — get it.
        if !ctx.detached {
            bwrap_cmd.with_die_with_parent();
        }
        bwrap_cmd.with_new_session();

        // =====================================================================
        // System directories (read-only)
        // =====================================================================
        bwrap_cmd
            .ro_bind_if_exists("/usr", "/usr")
            .ro_bind_if_exists("/lib", "/lib")
            .ro_bind_if_exists("/lib64", "/lib64")
            .ro_bind_if_exists("/bin", "/bin")
            .ro_bind_if_exists("/sbin", "/sbin")
            // DNS resolver config: gvproxy resolves `allow_net` hostnames
            // host-side (it runs in this shim) via the Go resolver, which reads
            // these. Without them the sandbox has no /etc/resolv.conf, every
            // lookup in buildAllowNetDNSZones fails, and allow-listed hosts
            // sinkhole to 0.0.0.0 — the allowlist silently blocks everything
            // whenever the jailer is enabled (#645).
            .ro_bind_if_exists("/etc/resolv.conf", "/etc/resolv.conf")
            .ro_bind_if_exists("/etc/hosts", "/etc/hosts")
            .ro_bind_if_exists("/etc/nsswitch.conf", "/etc/nsswitch.conf");

        // =====================================================================
        // Devices and special mounts
        // =====================================================================
        bwrap_cmd
            .with_dev()
            .dev_bind_if_exists("/dev/kvm", "/dev/kvm")
            .dev_bind_if_exists("/dev/net/tun", "/dev/net/tun")
            .with_proc()
            .tmpfs("/tmp");

        // =====================================================================
        // Bind all pre-computed paths (system dirs + user volumes)
        // =====================================================================
        for pa in ctx.writable_paths() {
            bwrap_cmd.bind(&pa.path, &pa.path);
            tracing::debug!(path = %pa.path.display(), "bwrap: bind (rw)");
        }
        for pa in ctx.readonly_paths() {
            bwrap_cmd.ro_bind(&pa.path, &pa.path);
            tracing::debug!(path = %pa.path.display(), "bwrap: ro-bind");
        }

        // =====================================================================
        // Environment sanitization
        // =====================================================================
        // The statically-linked shim dlopen's libkrunfw via LD_LIBRARY_PATH (its
        // `$ORIGIN` rpath is ineffective), and `--clearenv` wipes it — without
        // this the VM fails to start ("Couldn't find or load libkrunfw.so.5",
        // libkrun status=-2). Point it at the shim's own directory (`<box>/bin`),
        // which is bound into the sandbox and is exactly where `copy_libkrunfw`
        // placed the library the shim loads.
        let shim_dir = std::path::Path::new(&binary)
            .parent()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();

        bwrap_cmd
            .with_clearenv()
            .setenv("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .setenv("HOME", "/root")
            .setenv("LD_LIBRARY_PATH", shim_dir);

        // Preserve debugging environment variables
        if let Ok(rust_log) = std::env::var("RUST_LOG") {
            bwrap_cmd.setenv("RUST_LOG", rust_log);
        }
        if let Ok(rust_backtrace) = std::env::var("RUST_BACKTRACE") {
            bwrap_cmd.setenv("RUST_BACKTRACE", rust_backtrace);
        }

        bwrap_cmd.chdir("/");

        // Replace the command with bwrap-wrapped version.
        *cmd = bwrap_cmd.build(std::path::Path::new(&binary), &args);

        // Cgroup join is added by the Jailer's pre_exec hook (see
        // Jailer::command), decoupled from the bwrap sandbox.
    }

    fn name(&self) -> &'static str {
        "bwrap"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::advanced_options::ResourceLimits;

    /// The shim is statically linked, so libkrun's `dlopen` of `libkrunfw.so.5`
    /// can only be satisfied via `LD_LIBRARY_PATH` inside the `--clearenv`
    /// sandbox — the shim's `$ORIGIN` rpath is absent and the inherited
    /// `LD_LIBRARY_PATH` is wiped by `--clearenv`. Without this the VM fails to
    /// start ("Couldn't find or load libkrunfw.so.5", libkrun status=-2). This
    /// guards the env var the composable `apply()` dropped relative to the
    /// legacy `build_shim_command`.
    #[test]
    fn apply_sets_ld_library_path_to_shim_dir() {
        if !bwrap::is_available() {
            eprintln!("skipping apply_sets_ld_library_path_to_shim_dir: bwrap not available");
            return;
        }

        let limits = Box::leak(Box::new(ResourceLimits::default()));
        let ctx = SandboxContext {
            id: "test-box",
            paths: vec![],
            unix_sockets: Default::default(),
            resource_limits: limits,
            network_enabled: false,
            sandbox_profile: None,
            detached: false,
        };

        let shim = "/var/lib/boxlite/boxes/abc/bin/boxlite-shim";
        let mut cmd = Command::new(shim);
        BwrapSandbox::new().apply(&ctx, &mut cmd);

        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        let pos = args
            .windows(3)
            .position(|w| w[0] == "--setenv" && w[1] == "LD_LIBRARY_PATH")
            .expect("bwrap must --setenv LD_LIBRARY_PATH so the static shim can dlopen libkrunfw");
        assert_eq!(
            args[pos + 2],
            "/var/lib/boxlite/boxes/abc/bin",
            "LD_LIBRARY_PATH must point at the shim's own directory (where libkrunfw is copied)"
        );
    }

    /// A detached box must outlive the launcher, so it must NOT get bwrap's
    /// `--die-with-parent` (PR_SET_PDEATHSIG kills the shim/VM the instant
    /// `run -d` returns, leaving the box born-Stopped). Foreground boxes keep it
    /// so they die with their launcher.
    #[test]
    fn apply_sets_die_with_parent_only_for_foreground() {
        if !bwrap::is_available() {
            eprintln!(
                "skipping apply_sets_die_with_parent_only_for_foreground: bwrap not available"
            );
            return;
        }

        fn has_die_with_parent(detached: bool) -> bool {
            let limits = Box::leak(Box::new(ResourceLimits::default()));
            let ctx = SandboxContext {
                id: "test-box",
                paths: vec![],
                unix_sockets: Default::default(),
                resource_limits: limits,
                network_enabled: false,
                sandbox_profile: None,
                detached,
            };
            let mut cmd = Command::new("/var/lib/boxlite/boxes/abc/bin/boxlite-shim");
            BwrapSandbox::new().apply(&ctx, &mut cmd);
            cmd.get_args().any(|a| a == "--die-with-parent")
        }

        assert!(
            has_die_with_parent(false),
            "foreground box must get --die-with-parent so it dies with its launcher"
        );
        assert!(
            !has_die_with_parent(true),
            "detached box must not get --die-with-parent or it is killed when run -d returns"
        );
    }
}
