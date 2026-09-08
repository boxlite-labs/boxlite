use crate::cli::{
    CapabilityFlags, GlobalFlags, KernelFlags, ManagementFlags, NetworkFlags, ProcessFlags,
    PublishFlags, ResourceFlags, VolumeFlags,
};
use crate::terminal::StreamManager;
use crate::util::to_shell_exit_code;
use boxlite::{AttachOptions, BoxOptions, BoxliteRuntime, LiteBox, RootfsSpec};
use clap::Args;
use std::io::{self, IsTerminal};

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub process: ProcessFlags,

    #[command(flatten)]
    pub resource: ResourceFlags,

    #[command(flatten)]
    pub capability: CapabilityFlags,

    #[command(flatten)]
    pub publish: PublishFlags,

    #[command(flatten)]
    pub volume: VolumeFlags,

    #[command(flatten)]
    pub network: NetworkFlags,

    #[command(flatten)]
    pub management: ManagementFlags,

    /// Run the box in the background (detach)
    #[arg(short = 'd', long)]
    pub detach: bool,

    /// Automatically remove the box when it exits. Conflicts with lifecycle
    /// deadlines; ignored with `--detach` so the box outlives this CLI.
    #[arg(long, conflicts_with_all = ["auto_delete", "auto_stop"])]
    pub rm: bool,

    /// Override the image entrypoint with a single executable. Any trailing
    /// COMMAND replaces the image CMD and is appended to the entrypoint.
    #[arg(long = "entrypoint", value_name = "EXEC")]
    pub entrypoint: Option<String>,

    /// Path to an already prepared rootfs
    #[arg(long = "rootfs", value_name = "PATH")]
    pub rootfs: Option<String>,

    /// Image and command, or command only when --rootfs is set
    #[arg(index = 1, trailing_var_arg = true, value_name = "IMAGE|COMMAND")]
    pub args: Vec<String>,

    #[command(flatten, next_help_heading = "Advanced boot options")]
    pub boot: KernelFlags,
}

/// Entry point.
///
/// Returns the shell exit code the CLI should exit with (0 on success, the
/// box's mapped exit code on a non-zero command exit). Returning the code —
/// instead of calling `std::process::exit` mid-function — lets `BoxRunner`
/// (and the `BoxliteRuntime` it owns) drop normally, so `RuntimeImpl::Drop`
/// runs `shutdown_sync()` and stops the box's shim on every return path.
/// `std::process::exit` would bypass that Drop chain and leak the shim (#622).
pub async fn execute(args: RunArgs, global: &GlobalFlags) -> anyhow::Result<i32> {
    args.boot.require_enabled(global.experimental_features())?;
    args.management
        .require_enabled(global.experimental_features())?;
    args.management.require_sweeper(global.targets_rest()?)?;
    let (rootfs, command_args) = args.rootfs_and_command()?;
    let command_args = command_args.to_vec();
    let mut runner = BoxRunner::new(args, global)?;
    runner.run(rootfs, command_args).await
}

struct BoxRunner {
    args: RunArgs,
    rt: BoxliteRuntime,
}

/// Resolve the flags into box options.
///
/// A free function rather than a `BoxRunner` method so the flag-resolution
/// rules can be tested without a runtime: `BoxRunner` owns one, and
/// `create_box` ends in `rt.create`, which needs a VM.
fn build_options(
    args: &RunArgs,
    rootfs: RootfsSpec,
    command_args: &[String],
) -> anyhow::Result<BoxOptions> {
    let mut options = BoxOptions::default();
    args.resource.apply_to(&mut options);
    args.capability.apply_to(&mut options);
    args.boot.apply_to(&mut options);
    args.management.apply_to(&mut options)?;
    args.publish.apply_to(&mut options)?;
    args.volume.apply_to(&mut options)?;
    args.network.apply_to(&mut options)?;
    args.process.apply_to(&mut options)?;

    options.detach = args.detach;
    if args.rm {
        options.auto_delete = Some(1);
    }
    if let Some(ref exec) = args.entrypoint {
        options.entrypoint = Some(vec![exec.clone()]);
    }

    // Detached boxes keep manual lifecycle control: detach silently overrides
    // `--rm` (historical CLI behaviour). An explicit `--auto-delete` is a
    // deferred deadline, not remove-on-stop, so it survives detaching — that
    // pairing is the point of the flag.
    if args.detach && args.management.auto_delete.is_none() {
        options.auto_delete = Some(0);
    }

    // Docker semantics: the user COMMAND replaces the image CMD (the image
    // ENTRYPOINT is preserved and prepended) and the result runs as the
    // container's init. No COMMAND → the image default runs.
    if !command_args.is_empty() {
        options.cmd = Some(command_args.to_vec());
    }

    options.rootfs = rootfs;
    Ok(options)
}

impl BoxRunner {
    fn new(args: RunArgs, global: &GlobalFlags) -> anyhow::Result<Self> {
        let rt = global.create_runtime()?;

        Ok(Self { args, rt })
    }

    async fn run(&mut self, rootfs: RootfsSpec, command_args: Vec<String>) -> anyhow::Result<i32> {
        // Validate flags and environment
        self.validate_flags()?;

        // COMMAND becomes the container's init (docker semantics — it replaces
        // the image CMD via options.cmd in create_box), so there is nothing to
        // spawn here: attach to the init session instead.
        let litebox = self.create_box(rootfs, &command_args).await?;

        // Detach mode: start it and get out of the way. Nobody is reading the
        // output, so there is nothing to be attached for.
        if self.args.detach {
            litebox.start().await?;
            println!("{}", litebox.id());
            return Ok(0);
        }

        // Foreground: attach *before* the command runs, then start it. Starting
        // first races it — `run alpine echo hi` can finish before the attach
        // lands, and its output and exit code die with the VM. Attaching only
        // creates the container; `start()` runs its init.
        let mut execution = litebox.attach(AttachOptions::main()).await?;
        litebox.start().await?;

        // --tty implies --interactive when stdin is a terminal
        // (validate_flags already ensures stdin is a terminal when --tty is set)
        if self.args.process.tty {
            self.args.process.interactive = true;
        }

        // IO streaming and signal handling via shared StreamManager
        let streamer = StreamManager::new(
            &mut execution,
            self.args.process.interactive,
            self.args.process.tty,
        );

        let exit_code = streamer.start().await?;
        // Just return the shell exit code. Returning (vs. calling
        // `std::process::exit` here) lets `execution`, `litebox`, and the
        // owning `BoxliteRuntime` drop normally, so `RuntimeImpl::Drop`
        // runs `shutdown_sync()` and tears the box's shim down — the RAII
        // teardown the success path already relied on. `std::process::exit`
        // bypasses Drop entirely and leaked the shim on the non-zero path
        // (#622).
        Ok(to_shell_exit_code(exit_code))
    }

    async fn create_box(
        &self,
        rootfs: RootfsSpec,
        command_args: &[String],
    ) -> anyhow::Result<LiteBox> {
        let options = build_options(&self.args, rootfs, command_args)?;

        let litebox = self
            .rt
            .create(options, self.args.management.name.clone())
            .await?;

        Ok(litebox)
    }

    fn validate_flags(&self) -> anyhow::Result<()> {
        // Check TTY availability if requested
        if self.args.process.tty && !io::stdin().is_terminal() {
            anyhow::bail!("the input device is not a TTY.");
        }

        Ok(())
    }
}

impl RunArgs {
    fn rootfs_and_command(&self) -> anyhow::Result<(RootfsSpec, &[String])> {
        resolve_rootfs_and_command(self.rootfs.as_deref(), &self.args)
    }
}

fn resolve_rootfs_and_command<'a>(
    rootfs: Option<&str>,
    args: &'a [String],
) -> anyhow::Result<(RootfsSpec, &'a [String])> {
    if let Some(path) = rootfs {
        return Ok((RootfsSpec::RootfsPath(path.to_string()), args));
    }

    let Some((image, command)) = args.split_first() else {
        anyhow::bail!("provide IMAGE or --rootfs PATH");
    };

    Ok((RootfsSpec::Image(image.clone()), command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    /// The second consuming boundary. `run -d` clears the `--rm` encoding
    /// after `apply_to` has run, so it can clobber an explicit deadline exactly
    /// as `create` could — and this pairing is what the CLI reference promises:
    /// "an explicit `--auto-delete` survives detaching".
    #[test]
    fn an_explicit_delete_deadline_survives_run_detach() {
        let cli = Cli::try_parse_from([
            "boxlite",
            "run",
            "-d",
            "--auto-delete",
            "1h",
            "alpine:latest",
        ])
        .expect("run -d --auto-delete should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };
        let (rootfs, command_args) = args.rootfs_and_command().expect("rootfs resolves");
        let command_args = command_args.to_vec();

        let opts = build_options(&args, rootfs, &command_args).expect("options build");

        assert_eq!(opts.auto_delete, Some(3_600));
        assert!(opts.detach);
    }

    #[test]
    fn run_detach_without_a_deadline_clears_the_rm_encoding() {
        let cli = Cli::try_parse_from(["boxlite", "run", "-d", "--rm", "alpine:latest"])
            .expect("run -d --rm should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };
        let (rootfs, command_args) = args.rootfs_and_command().expect("rootfs resolves");
        let command_args = command_args.to_vec();

        let opts = build_options(&args, rootfs, &command_args).expect("options build");

        assert_eq!(
            opts.auto_delete,
            Some(0),
            "a detached box keeps manual lifecycle control, so --rm is dropped"
        );
    }

    #[test]
    fn run_capability_flags_are_repeatable() {
        let cli = Cli::try_parse_from([
            "boxlite",
            "run",
            "--cap-add",
            "SYS_ADMIN",
            "--cap-add",
            "NET_ADMIN",
            "--cap-drop",
            "CAP_NET_RAW",
            "alpine",
        ])
        .expect("capability flags should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert_eq!(args.capability.cap_add, vec!["SYS_ADMIN", "NET_ADMIN"]);
        assert_eq!(args.capability.cap_drop, vec!["CAP_NET_RAW"]);
    }

    #[test]
    fn run_rootfs_flag_sets_rootfs_path_and_uses_trailing_command() {
        let cli = Cli::try_parse_from(["boxlite", "run", "--rootfs", "/tmp/rootfs", "echo", "hi"])
            .expect("run --rootfs should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };

        let (rootfs, command) = args
            .rootfs_and_command()
            .expect("rootfs command should resolve");

        match rootfs {
            RootfsSpec::RootfsPath(path) => assert_eq!(path, "/tmp/rootfs"),
            other => panic!("expected RootfsPath, got {other:?}"),
        }
        assert_eq!(command, &["echo".to_string(), "hi".to_string()]);
    }

    #[test]
    fn run_rootfs_without_command_leaves_command_empty() {
        let cli = Cli::try_parse_from(["boxlite", "run", "--rootfs", "/tmp/rootfs"])
            .expect("run --rootfs should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };

        let (rootfs, command) = args
            .rootfs_and_command()
            .expect("rootfs command should resolve");

        match rootfs {
            RootfsSpec::RootfsPath(path) => assert_eq!(path, "/tmp/rootfs"),
            other => panic!("expected RootfsPath, got {other:?}"),
        }
        // No COMMAND → empty; the image/rootfs default init runs (docker
        // semantics), rather than the CLI forcing a shell.
        assert!(command.is_empty());
    }

    #[test]
    fn run_without_rootfs_preserves_image_and_command() {
        let cli = Cli::try_parse_from(["boxlite", "run", "alpine:latest", "echo", "hi"])
            .expect("run image command should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };

        let (rootfs, command) = args
            .rootfs_and_command()
            .expect("image command should resolve");

        match rootfs {
            RootfsSpec::Image(image) => assert_eq!(image, "alpine:latest"),
            other => panic!("expected Image, got {other:?}"),
        }
        assert_eq!(command, &["echo".to_string(), "hi".to_string()]);
    }

    #[test]
    fn run_requires_image_or_rootfs() {
        let cli = Cli::try_parse_from(["boxlite", "run"]).expect("run should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };

        let err = args
            .rootfs_and_command()
            .expect_err("missing source must be rejected");

        assert!(err.to_string().contains("IMAGE or --rootfs"));
    }
}
