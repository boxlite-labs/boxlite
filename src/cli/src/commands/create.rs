use crate::cli::{
    CapabilityFlags, GlobalFlags, KernelFlags, NetworkFlags, PublishFlags, ResourceFlags,
    VolumeFlags,
};
use boxlite::{BoxOptions, RootfsSpec};
use clap::Args;

/// Create a new box
#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Image to create from
    #[arg(index = 1)]
    pub image: Option<String>,

    /// Path to an already prepared rootfs
    #[arg(long = "rootfs", value_name = "PATH")]
    pub rootfs: Option<String>,

    #[command(flatten)]
    pub management: crate::cli::ManagementFlags,

    /// Set environment variables
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,

    /// Working directory inside the box
    #[arg(short = 'w', long = "workdir")]
    pub workdir: Option<String>,

    /// Override the image entrypoint with a single executable, mirroring
    /// `docker create --entrypoint`.
    #[arg(long = "entrypoint", value_name = "EXEC")]
    pub entrypoint: Option<String>,

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

    /// Command to run as the container's init (replaces the image CMD;
    /// the image ENTRYPOINT is preserved), mirroring `docker create`.
    #[arg(index = 2, trailing_var_arg = true)]
    pub command: Vec<String>,

    #[command(flatten, next_help_heading = "Advanced boot options")]
    pub boot: KernelFlags,
}

pub async fn execute(args: CreateArgs, global: &GlobalFlags) -> anyhow::Result<()> {
    let box_options = args.to_box_options(global)?;
    let rt = global.create_runtime()?;

    let litebox = rt.create(box_options, args.management.name.clone()).await?;
    println!("{}", litebox.id());

    Ok(())
}

impl CreateArgs {
    fn to_box_options(&self, global: &GlobalFlags) -> anyhow::Result<BoxOptions> {
        self.boot.require_enabled(global.experimental_features())?;
        self.management
            .require_enabled(global.experimental_features())?;
        self.management.require_sweeper(global.targets_rest()?)?;
        let mut options = BoxOptions::default();
        self.resource.apply_to(&mut options);
        self.capability.apply_to(&mut options);
        self.boot.apply_to(&mut options);
        self.management.apply_to(&mut options)?;
        self.publish.apply_to(&mut options)?;
        self.volume.apply_to(&mut options)?;
        self.network.apply_to(&mut options)?;

        // A `create`d box is a background box: `create` then `start`/`exec` runs
        // its main command detached (docker's create → start), so the launching
        // CLI's exit must not tear it down. Foreground/interactive lifecycles go
        // through `run` instead, which sets detach from `-d`. Without this, a
        // non-detached created box is killed by the exiting `start` CLI (its
        // watchdog + the runtime's drop-time auto-stop) before its main command
        // records an exit code — the box then reports 0 instead of its real
        // code. Remove-on-stop belongs to `run`; create exposes only deferred
        // lifecycle deadlines, which a server can enforce after this CLI exits.
        options.detach = true;
        options.working_dir = self.workdir.clone();
        if let Some(ref exec) = self.entrypoint {
            options.entrypoint = Some(vec![exec.clone()]);
        }
        if !self.command.is_empty() {
            options.cmd = Some(self.command.clone());
        }
        crate::cli::apply_env_vars(&self.env, &mut options);
        options.rootfs = self.rootfs_spec()?;
        Ok(options)
    }

    fn rootfs_spec(&self) -> anyhow::Result<RootfsSpec> {
        match (self.image.as_ref(), self.rootfs.as_ref()) {
            (Some(image), None) => Ok(RootfsSpec::Image(image.clone())),
            (None, Some(path)) => Ok(RootfsSpec::RootfsPath(path.clone())),
            (None, None) => anyhow::bail!("provide IMAGE or --rootfs PATH"),
            (Some(_), Some(_)) => anyhow::bail!("provide either IMAGE or --rootfs PATH, not both"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    /// `ManagementFlags::apply_to` writes the keep-on-stop sentinel before an
    /// optional deadline. `create` then sets detach without changing lifecycle
    /// policy, so the explicit deadline must survive this consuming boundary.
    #[test]
    fn an_explicit_delete_deadline_survives_creates_detach_default() {
        let cli = Cli::try_parse_from([
            "boxlite",
            "--url",
            "http://127.0.0.1:1",
            "create",
            "--auto-delete",
            "1h",
            "alpine:latest",
        ])
        .expect("create --auto-delete should parse");
        let Commands::Create(args) = cli.command else {
            panic!("expected create command");
        };

        let opts = args
            .to_box_options(&cli.global)
            .expect("options should build");

        assert!(opts.detach, "create always detaches");
        assert_eq!(opts.auto_delete, Some(3_600));
    }

    #[test]
    fn create_without_a_deadline_disables_remove_on_stop() {
        let opts = {
            let cli = Cli::try_parse_from(["boxlite", "create", "alpine:latest"])
                .expect("create should parse");
            let Commands::Create(args) = cli.command else {
                panic!("expected create command");
            };
            args.to_box_options(&cli.global)
                .expect("options should build")
        };

        assert_eq!(
            opts.auto_delete,
            Some(0),
            "a detached box cannot remove on stop, so the sentinel is cleared"
        );
    }

    #[test]
    fn create_rootfs_flag_sets_rootfs_path() {
        let cli = Cli::try_parse_from(["boxlite", "create", "--rootfs", "/tmp/rootfs"])
            .expect("create --rootfs should parse");
        let Commands::Create(args) = cli.command else {
            panic!("expected create command");
        };

        let opts = args
            .to_box_options(&cli.global)
            .expect("rootfs options should build");

        match opts.rootfs {
            RootfsSpec::RootfsPath(path) => assert_eq!(path, "/tmp/rootfs"),
            other => panic!("expected RootfsPath, got {other:?}"),
        }
    }

    #[test]
    fn create_requires_image_or_rootfs() {
        let cli = Cli::try_parse_from(["boxlite", "create"]).expect("create should parse");
        let Commands::Create(args) = cli.command else {
            panic!("expected create command");
        };

        let err = args
            .to_box_options(&cli.global)
            .expect_err("missing source must be rejected");

        assert!(err.to_string().contains("IMAGE or --rootfs"));
    }

    #[test]
    fn create_rejects_image_and_rootfs_together() {
        let cli = Cli::try_parse_from(["boxlite", "create", "--rootfs", "/tmp/rootfs", "alpine"])
            .expect("create image and rootfs should parse");
        let Commands::Create(args) = cli.command else {
            panic!("expected create command");
        };

        let err = args
            .to_box_options(&cli.global)
            .expect_err("competing sources must be rejected");

        assert!(err.to_string().contains("either IMAGE or --rootfs"));
    }

    #[test]
    fn create_capability_flags_reach_box_options() {
        let cli = Cli::try_parse_from([
            "boxlite",
            "create",
            "--cap-add",
            "SYS_ADMIN",
            "--cap-drop",
            "CAP_NET_RAW",
            "alpine",
        ])
        .expect("capability flags should parse");
        let Commands::Create(args) = cli.command else {
            panic!("expected create command");
        };

        let opts = args
            .to_box_options(&cli.global)
            .expect("options should build");
        let capabilities = opts.advanced.capabilities().expect("capabilities set");
        assert_eq!(capabilities.add, vec!["SYS_ADMIN"]);
        assert_eq!(capabilities.drop, vec!["CAP_NET_RAW"]);
    }
}
