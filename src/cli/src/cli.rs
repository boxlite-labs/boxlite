//! CLI definition and argument parsing for boxlite-cli.
//! This module contains all CLI-related code including the main CLI structure,
//! subcommands, and flag definitions.

use anyhow::Context;
use boxlite::experimental::custom_kernel::{KernelFormat, KernelOptions, configure};
use boxlite::experimental::{
    EXPERIMENTAL_FEATURES_ENV, ExperimentalFeature, ExperimentalFeatures, RuntimeBuilder,
};
use boxlite::runtime::options::{
    InboundNetworkConfig, NetBandwidth, NetworkMode, OutboundNetworkConfig, PortProtocol, PortSpec,
    VolumeSpec,
};
use boxlite::{
    BoxCommand, BoxOptions, BoxliteOptions, BoxliteRestOptions, BoxliteRuntime,
    ContainerCapabilities, ImageRegistry, NetworkSpec,
};
use clap::error::ErrorKind;
use clap::parser::ValueSource;
use clap::{
    Arg, ArgMatches, Args, Command, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum,
};
use clap_complete::shells::{Bash, Fish, Zsh};
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Helper to parse CLI environment variables and apply them to BoxOptions
pub fn apply_env_vars(env: &[String], opts: &mut BoxOptions) {
    apply_env_vars_with_lookup(env, opts, |k| std::env::var(k).ok())
}

/// Helper to parse CLI environment variables with custom lookup for host variables
pub fn apply_env_vars_with_lookup<F>(env: &[String], opts: &mut BoxOptions, lookup: F)
where
    F: Fn(&str) -> Option<String>,
{
    opts.env.extend(resolve_env_vars_with_lookup(env, lookup));
}

fn resolve_env_vars_with_lookup<F>(env: &[String], lookup: F) -> Vec<(String, String)>
where
    F: Fn(&str) -> Option<String>,
{
    let mut resolved = Vec::with_capacity(env.len());
    for env_str in env {
        if let Some((k, v)) = env_str.split_once('=') {
            resolved.push((k.to_string(), v.to_string()));
        } else if let Some(val) = lookup(env_str) {
            resolved.push((env_str.to_string(), val));
        } else {
            tracing::warn!(
                "Environment variable '{}' not found on host, skipping",
                env_str
            );
        }
    }
    resolved
}

// ============================================================================
// CLI Definition
// ============================================================================

#[derive(Parser, Debug)]
#[command(name = "boxlite", author, version, about = "BoxLite CLI")]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalFlags,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GlobalOption {
    Debug,
    Home,
    Registry,
    Config,
    Url,
    Profile,
    PathPrefix,
}

impl GlobalOption {
    const ALL: &'static [Self] = &[
        Self::Debug,
        Self::Home,
        Self::Registry,
        Self::Config,
        Self::Url,
        Self::Profile,
        Self::PathPrefix,
    ];

    const fn id(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Home => "home",
            Self::Registry => "registry",
            Self::Config => "config",
            Self::Url => "url",
            Self::Profile => "profile",
            Self::PathPrefix => "path_prefix",
        }
    }

    const fn long(self) -> &'static str {
        match self {
            Self::PathPrefix => "path-prefix",
            _ => self.id(),
        }
    }

    const fn propagates_value(self) -> bool {
        !matches!(self, Self::Registry)
    }

    fn clear(self, global: &mut GlobalFlags) {
        match self {
            Self::Debug => global.debug = false,
            Self::Home => global.home = None,
            Self::Registry => global.registry.clear(),
            Self::Config => global.config = None,
            Self::Url => global.url = None,
            Self::Profile => global.profile = None,
            Self::PathPrefix => global.path_prefix = None,
        }
    }
}

#[derive(Debug)]
struct CommandScope {
    name: &'static str,
    globals: &'static [GlobalOption],
    children: &'static [CommandScope],
}

impl CommandScope {
    fn allows(&self, option: GlobalOption) -> bool {
        self.globals.contains(&option)
    }
}

const ALL_GLOBALS: &[GlobalOption] = GlobalOption::ALL;
const BOX_GLOBALS: &[GlobalOption] = &[
    GlobalOption::Debug,
    GlobalOption::Home,
    GlobalOption::Config,
    GlobalOption::Url,
    GlobalOption::Profile,
    GlobalOption::PathPrefix,
];
const LOCAL_IMAGE_GLOBALS: &[GlobalOption] = &[
    GlobalOption::Debug,
    GlobalOption::Home,
    GlobalOption::Registry,
    GlobalOption::Config,
];
const LOCAL_GLOBALS: &[GlobalOption] = &[
    GlobalOption::Debug,
    GlobalOption::Home,
    GlobalOption::Config,
];
const AUTH_GLOBALS: &[GlobalOption] = &[
    GlobalOption::Debug,
    GlobalOption::Home,
    GlobalOption::Profile,
];
const AUTH_URL_GLOBALS: &[GlobalOption] = &[
    GlobalOption::Debug,
    GlobalOption::Home,
    GlobalOption::Url,
    GlobalOption::Profile,
];
const VOLUME_GLOBALS: &[GlobalOption] = &[
    GlobalOption::Debug,
    GlobalOption::Home,
    GlobalOption::Url,
    GlobalOption::Profile,
    GlobalOption::PathPrefix,
];

const AUTH_SCOPES: &[CommandScope] = &[
    CommandScope {
        name: "login",
        globals: AUTH_URL_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "logout",
        globals: AUTH_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "status",
        globals: AUTH_URL_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "whoami",
        globals: AUTH_URL_GLOBALS,
        children: &[],
    },
];
const NETWORK_SCOPES: &[CommandScope] = &[CommandScope {
    name: "tunnel",
    globals: BOX_GLOBALS,
    children: &[],
}];
const VOLUME_SCOPES: &[CommandScope] = &[
    CommandScope {
        name: "create",
        globals: VOLUME_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "ls",
        globals: VOLUME_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "get",
        globals: VOLUME_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "rm",
        globals: VOLUME_GLOBALS,
        children: &[],
    },
];
const COMMAND_SCOPES: &[CommandScope] = &[
    CommandScope {
        name: "run",
        globals: ALL_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "exec",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "create",
        globals: ALL_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "list",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "rm",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "start",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "stop",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "restart",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "pull",
        globals: LOCAL_IMAGE_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "images",
        globals: LOCAL_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "inspect",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "cp",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "info",
        globals: LOCAL_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "logs",
        globals: LOCAL_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "stats",
        globals: BOX_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "network",
        globals: BOX_GLOBALS,
        children: NETWORK_SCOPES,
    },
    CommandScope {
        name: "serve",
        globals: LOCAL_IMAGE_GLOBALS,
        children: &[],
    },
    CommandScope {
        name: "auth",
        globals: AUTH_GLOBALS,
        children: AUTH_SCOPES,
    },
    CommandScope {
        name: "volume",
        globals: VOLUME_GLOBALS,
        children: VOLUME_SCOPES,
    },
    CommandScope {
        name: "completion",
        globals: &[],
        children: &[],
    },
];

/// Build one command tree for runtime parsing, help, and completion.
///
/// Root copies preserve `boxlite --option command` compatibility but are not
/// global. Each command receives only the positive set it owns, so Clap itself
/// rejects an irrelevant option written after the command and renders the same
/// scope in help. Environment bindings live on those scoped copies so an
/// irrelevant ambient value is never parsed. Singular values use Clap's
/// documented propagation back to the root `GlobalFlags`; repeatable values
/// are folded explicitly because propagation selects one level's `MatchedArg`
/// rather than appending values from independently defined levels.
pub fn command() -> Command {
    let command = Cli::command();
    let global_args = command
        .get_arguments()
        .filter(|argument| argument.is_global_set())
        .cloned()
        .collect::<Vec<_>>();
    let command = command.mut_args(|argument| {
        if argument.is_global_set() {
            argument.global(false).env(None::<&'static str>)
        } else {
            argument
        }
    });
    apply_command_scopes(command, COMMAND_SCOPES, &global_args)
}

fn apply_command_scopes(
    command: Command,
    scopes: &'static [CommandScope],
    global_args: &[Arg],
) -> Command {
    for subcommand in command.get_subcommands() {
        assert!(
            scopes
                .iter()
                .any(|scope| scope.name == subcommand.get_name()),
            "missing global-option scope for `boxlite {}`",
            subcommand.get_name()
        );
    }
    for scope in scopes {
        assert!(
            command
                .get_subcommands()
                .any(|subcommand| subcommand.get_name() == scope.name),
            "global-option scope references missing command `{}`",
            scope.name
        );
    }

    command.mut_subcommands(|subcommand| {
        let scope = scopes
            .iter()
            .find(|scope| scope.name == subcommand.get_name())
            .expect("command scope checked above");
        let local_ids = subcommand
            .get_arguments()
            .map(|argument| argument.get_id().clone())
            .collect::<Vec<_>>();
        let scoped_args = global_args
            .iter()
            .filter(|argument| {
                scope
                    .globals
                    .iter()
                    .any(|option| option.id() == argument.get_id().as_str())
            })
            .filter(|argument| !local_ids.contains(argument.get_id()))
            .cloned()
            .map(|argument| {
                let option = GlobalOption::ALL
                    .iter()
                    .copied()
                    .find(|option| option.id() == argument.get_id().as_str())
                    .expect("scoped argument must be a declared global option");
                argument.global(option.propagates_value())
            });
        apply_command_scopes(subcommand.args(scoped_args), scope.children, global_args)
    })
}

/// Parse through [`command`] so the runtime parser and rendered help use the
/// same command-capability policy.
pub fn parse() -> Cli {
    try_parse_from(std::env::args_os()).unwrap_or_else(|error| error.exit())
}

pub(crate) fn try_parse_from<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut parser = command();
    let mut matches = parser.clone().try_get_matches_from(args)?;
    let command_path = selected_command_path(&matches);
    let scope = command_scope(&command_path).expect("parsed command must have a declared scope");

    if let Some(option) = GlobalOption::ALL.iter().copied().find(|option| {
        !scope.allows(*option)
            && matches.value_source(option.id()) == Some(ValueSource::CommandLine)
    }) {
        return Err(irrelevant_global_error(&mut parser, &command_path, option));
    }

    if let Some(home) = matches.get_one::<PathBuf>("home")
        && scope.allows(GlobalOption::Home)
        && !home.is_absolute()
    {
        return Err(parser.error(
            ErrorKind::ValueValidation,
            format!(
                "BoxLite home must be an absolute path, got: {}",
                home.display()
            ),
        ));
    }

    for id in ["url", "profile", "path_prefix", "config"] {
        if scope.globals.iter().any(|option| option.id() == id)
            && matches.value_source(id) == Some(ValueSource::CommandLine)
            && matches.get_one::<String>(id).is_some_and(String::is_empty)
        {
            let option = id.replace('_', "-");
            return Err(parser.error(
                ErrorKind::ValueValidation,
                format!("--{option} cannot be empty"),
            ));
        }
    }
    let registry_values = command_line_values_along_selected_path(&matches, "registry");
    if scope.allows(GlobalOption::Registry) && registry_values.iter().any(String::is_empty) {
        return Err(parser.error(ErrorKind::ValueValidation, "--registry cannot be empty"));
    }

    let mut cli = Cli::from_arg_matches_mut(&mut matches)?;
    if scope.allows(GlobalOption::Registry) {
        cli.global.registry = registry_values;
    }
    clear_irrelevant_environment_globals(&mut cli.global, scope);
    merge_root_login_url(&mut cli);
    normalize_empty_urls(&mut cli);
    Ok(cli)
}

fn command_line_values_along_selected_path(matches: &ArgMatches, id: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = matches;
    loop {
        if let Ok(Some(level_values)) = current.try_get_many::<String>(id) {
            values.extend(level_values.cloned());
        }
        let Some((_, child)) = current.subcommand() else {
            break;
        };
        current = child;
    }
    values
}

fn selected_command_path(matches: &ArgMatches) -> Vec<&str> {
    let mut path = Vec::new();
    let mut current = matches;
    while let Some((name, child)) = current.subcommand() {
        path.push(name);
        current = child;
    }
    path
}

fn command_scope(path: &[&str]) -> Option<&'static CommandScope> {
    let mut scopes = COMMAND_SCOPES;
    let mut selected = None;
    for name in path {
        let scope = scopes.iter().find(|scope| scope.name == *name)?;
        selected = Some(scope);
        scopes = scope.children;
    }
    selected
}

fn irrelevant_global_error(
    parser: &mut Command,
    command_path: &[&str],
    option: GlobalOption,
) -> clap::Error {
    parser.error(
        ErrorKind::UnknownArgument,
        format!(
            "--{} is not valid for `boxlite {}`",
            option.long(),
            command_path.join(" ")
        ),
    )
}

/// Irrelevant values supplied by ambient environment variables are ignored:
/// they are not command options and must not make an unrelated command fail.
fn clear_irrelevant_environment_globals(global: &mut GlobalFlags, scope: &CommandScope) {
    for option in GlobalOption::ALL {
        if !scope.allows(*option) {
            option.clear(global);
        }
    }
}

fn merge_root_login_url(cli: &mut Cli) {
    if let Commands::Auth(crate::commands::auth::AuthArgs {
        command: crate::commands::auth::AuthCommand::Login(login),
    }) = &mut cli.command
        && login.url.is_none()
    {
        login.url.clone_from(&cli.global.url);
    }
}

fn normalize_empty_urls(cli: &mut Cli) {
    if cli.global.url.as_deref() == Some("") {
        cli.global.url = None;
    }
    if let Commands::Auth(crate::commands::auth::AuthArgs {
        command: crate::commands::auth::AuthCommand::Login(login),
    }) = &mut cli.command
        && login.url.as_deref() == Some("")
    {
        login.url = None;
    }
}

#[derive(Subcommand, Debug)]
#[non_exhaustive]
pub enum Commands {
    /// Run a command in a new box
    Run(crate::commands::run::RunArgs),
    /// Execute a command in a running box
    Exec(crate::commands::exec::ExecArgs),
    /// Create a new box
    Create(crate::commands::create::CreateArgs),

    /// List boxes
    #[command(visible_alias = "ls", visible_alias = "ps")]
    List(crate::commands::list::ListArgs),

    /// Remove one or more boxes
    Rm(crate::commands::rm::RmArgs),

    /// Start one or more stopped boxes
    Start(crate::commands::start::StartArgs),

    /// Stop one or more running boxes
    Stop(crate::commands::stop::StopArgs),

    /// Restart one or more boxes
    Restart(crate::commands::restart::RestartArgs),

    /// Pull an image from a registry
    Pull(crate::commands::pull::PullArgs),

    /// List images
    Images(crate::commands::images::ImagesArgs),

    /// Display detailed information on a box
    Inspect(crate::commands::inspect::InspectArgs),

    /// Copy files/folders between host and box
    Cp(crate::commands::cp::CpArgs),

    /// Display system-wide runtime information
    Info(crate::commands::info::InfoArgs),

    /// Show logs from a box
    Logs(crate::commands::logs::LogsArgs),

    /// Display resource usage statistics for a box
    Stats(crate::commands::stats::StatsArgs),

    /// Manage box networking
    Network(crate::commands::network::NetworkArgs),

    /// Start a long-running REST API server
    Serve(crate::commands::serve::ServeArgs),

    /// Authenticate with a remote BoxLite server
    Auth(crate::commands::auth::AuthArgs),

    /// Manage named volumes on a REST server
    Volume(crate::commands::volume::VolumeArgs),

    /// Generate shell completion script (hidden from help)
    #[command(hide = true)]
    Completion(CompletionArgs),
}

/// Shell for which to generate completion script.
#[derive(ValueEnum, Clone, Debug)]
#[value(rename_all = "lower")]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

/// Arguments for the completion subcommand.
#[derive(Args, Debug)]
pub struct CompletionArgs {
    /// Shell to generate completion for (bash, zsh, fish).
    pub shell: Shell,
}

/// Writes a completion script for the given shell to `out`.
pub fn generate_completion(shell: &Shell, name: &str, out: &mut dyn Write) {
    let mut cmd = completion_command();
    match shell {
        Shell::Bash => clap_complete::generate(Bash, &mut cmd, name, out),
        Shell::Zsh => clap_complete::generate(Zsh, &mut cmd, name, out),
        Shell::Fish => clap_complete::generate(Fish, &mut cmd, name, out),
    }
}

fn completion_command() -> Command {
    // clap_complete's AOT generators enumerate hidden arguments. Runtime,
    // help, and scoped globals already share `command()`; this final projection
    // removes only experimental arguments from completion discovery.
    command()
        .mut_subcommand("run", without_hidden_arguments)
        .mut_subcommand("create", without_hidden_arguments)
}

fn without_hidden_arguments(command: Command) -> Command {
    let name = match command.get_name() {
        "run" => "run",
        "create" => "create",
        other => panic!("completion projection is not defined for {other}"),
    };
    let about = command.get_about().cloned();
    let long_about = command.get_long_about().cloned();
    let display_order = command.get_display_order();
    let visible_arguments = command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .cloned()
        .collect::<Vec<_>>();
    let subcommands = command.get_subcommands().cloned().collect::<Vec<_>>();

    let mut projected = Command::new(name)
        .display_order(display_order)
        .args(visible_arguments)
        .subcommands(subcommands);
    if let Some(about) = about {
        projected = projected.about(about);
    }
    if let Some(long_about) = long_about {
        projected = projected.long_about(long_about);
    }
    projected
}

pub(crate) fn experimental_features_from_env() -> boxlite::BoxliteResult<ExperimentalFeatures> {
    match std::env::var(EXPERIMENTAL_FEATURES_ENV) {
        Ok(value) => ExperimentalFeatures::parse(&value),
        Err(std::env::VarError::NotPresent) => Ok(ExperimentalFeatures::default()),
        Err(std::env::VarError::NotUnicode(_)) => Err(boxlite::BoxliteError::Config(format!(
            "{EXPERIMENTAL_FEATURES_ENV} must contain valid UTF-8"
        ))),
    }
}

// ============================================================================
// GLOBAL FLAGS
// ============================================================================

#[derive(Args, Debug, Clone)]
pub struct GlobalFlags {
    /// Enable debug output
    #[arg(long, global = true)]
    pub debug: bool,

    /// Absolute BoxLite runtime and credential home directory
    #[arg(long, global = true, env = "BOXLITE_HOME")]
    pub home: Option<std::path::PathBuf>,

    /// Image registry to use (can be specified multiple times)
    #[arg(long, global = true, value_name = "REGISTRY")]
    pub registry: Vec<String>,

    /// Configuration file path (optional)
    ///
    /// Specifies the JSON configuration file containing BoxLite options such as image_registries.
    /// If not provided, uses default options (no config file is loaded from $BOXLITE_HOME).
    #[arg(long, global = true)]
    pub config: Option<String>,

    /// Connect to a remote BoxLite REST API server instead of local runtime.
    #[arg(long, global = true, env = "BOXLITE_REST_URL")]
    pub url: Option<String>,

    /// Named profile in `<BOXLITE_HOME>/credentials.toml`. Lets one machine
    /// hold separate remote logins. Defaults to `default` if neither flag nor
    /// env is set.
    #[arg(long, global = true, env = "BOXLITE_PROFILE")]
    pub profile: Option<String>,

    /// Routing-slot value for the URL path (`/v1/<prefix>/...`).
    /// Opaque — the server decides what this means (org id, workspace,
    /// catalog, …); the value typically comes from the `auth login`
    /// flow capturing `Principal.path_prefix`. This flag overrides
    /// the stored profile's path_prefix for users whose credential
    /// has scope over multiple routing values (e.g. multiple orgs on
    /// the same account). Unset → uses the stored profile's
    /// path_prefix, then empty (URL skips the segment —
    /// `/v1/boxes/...`).
    #[arg(long = "path-prefix", global = true, env = "BOXLITE_REST_PATH_PREFIX")]
    pub path_prefix: Option<String>,

    #[arg(skip)]
    pub(crate) experimental_features: ExperimentalFeatures,
}

impl GlobalFlags {
    pub(crate) fn credential_store(&self) -> crate::credentials::CredentialStore {
        crate::credentials::CredentialStore::new(self.home.clone())
    }

    /// Resolve which credential profile to read/write. Order: explicit
    /// `--profile` flag (which clap also fills from `BOXLITE_PROFILE`) > the
    /// hard-coded `default` name. Keeping this in one helper means a future
    /// "tab through last-used profile" UX has exactly one place to change.
    pub fn resolved_profile(&self) -> String {
        self.profile
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(crate::credentials::DEFAULT_PROFILE)
            .to_string()
    }

    pub fn experimental_features(&self) -> &ExperimentalFeatures {
        &self.experimental_features
    }

    /// Resolve runtime options from config file and CLI overrides (--home, --registry).
    pub fn resolve_runtime_options(&self) -> anyhow::Result<BoxliteOptions> {
        let mut options = if let Some(config_path) = &self.config {
            crate::config::load_config(Path::new(config_path))?
        } else {
            BoxliteOptions::default()
        };

        if let Some(cli_home) = &self.home {
            options.home_dir = cli_home.clone();
        }

        if !self.registry.is_empty() {
            options.image_registries = self
                .registry
                .iter()
                .map(|host| ImageRegistry::https(host).with_search(true))
                .chain(options.image_registries)
                .collect();
        }

        Ok(options)
    }

    /// Create a runtime from pre-resolved options (avoids resolving twice when caller already has options).
    pub fn create_runtime_with_options(
        &self,
        options: BoxliteOptions,
    ) -> anyhow::Result<BoxliteRuntime> {
        RuntimeBuilder::new(options)
            .with_features(self.experimental_features.clone())
            .build()
            .map_err(Into::into)
    }

    /// Whether this invocation talks to a server rather than the embedded
    /// runtime.
    ///
    /// Resolved exactly as `create_runtime` does, so the two cannot disagree
    /// about which backend a command is aimed at. It re-reads the credential
    /// file rather than threading the resolution through; the cost is one extra
    /// read on the two commands that ask.
    pub fn targets_rest(&self) -> anyhow::Result<bool> {
        let stored = self.load_selected_profile()?;
        let env_api_key = std::env::var("BOXLITE_API_KEY").ok();
        Ok(self.resolve_rest_options(stored, env_api_key).is_some())
    }

    pub fn create_runtime(&self) -> anyhow::Result<BoxliteRuntime> {
        let stored = self.load_selected_profile()?;
        // Clap reads BOXLITE_REST_URL into `self.url`; BOXLITE_API_KEY is the
        // one credential env we still consult directly here.
        let env_api_key = std::env::var("BOXLITE_API_KEY").ok();

        match self.resolve_rest_options(stored, env_api_key) {
            Some(opts) => BoxliteRuntime::rest(opts).map_err(Into::into),
            None => {
                // No URL anywhere → local runtime, unchanged behavior.
                let options = self.resolve_runtime_options()?;
                self.create_runtime_with_options(options)
            }
        }
    }

    fn load_selected_profile(&self) -> anyhow::Result<Option<crate::credentials::Profile>> {
        let profile = self.resolved_profile();
        self.credential_store()
            .load_named(&profile)
            .with_context(|| format!("loading credential profile `{profile}`"))
    }

    /// Build REST connection options from the selected credential profile and
    /// the ambient `BOXLITE_API_KEY`. Returns `None` when no URL is configured
    /// (the caller then falls back to the local runtime). Pure — takes the
    /// resolved profile and env key as arguments and touches neither disk nor
    /// process environment — so the precedence below is unit-testable.
    ///
    /// Precedence (each axis independent):
    /// - URL: `--url` / `BOXLITE_REST_URL` > stored profile.
    /// - routing slot (`path_prefix`): `--path-prefix` /
    ///   `BOXLITE_REST_PATH_PREFIX` > stored profile.
    /// - bearer credential: `BOXLITE_API_KEY` > stored profile.
    ///
    /// `BOXLITE_API_KEY` overrides ONLY the bearer credential — the selected
    /// profile's url and path_prefix still apply, so `--profile p1` keeps
    /// routing to its tenant (`/v1/<prefix>/…`) even with an ambient key set.
    /// Building the options bare in that branch (instead of starting from the
    /// profile) was the cause of the prefix-less `/v1/boxes` 404 against a
    /// multi-tenant server.
    pub(crate) fn resolve_rest_options(
        &self,
        stored: Option<crate::credentials::Profile>,
        env_api_key: Option<String>,
    ) -> Option<BoxliteRestOptions> {
        let url = self
            .url
            .clone()
            .or_else(|| stored.as_ref().map(|p| p.url.clone()))?;

        // Start from the stored profile so its url + path_prefix (routing
        // slot) survive; the env key below overrides only the bearer.
        let mut opts = match stored {
            Some(profile) => {
                let mut from_profile = crate::credentials::into_rest_options(profile);
                // `--url` (resolved above) wins over the stored URL.
                from_profile.url = self.url.clone().unwrap_or(from_profile.url);
                from_profile
            }
            None => BoxliteRestOptions::new(url),
        };

        if let Some(key) = env_api_key.filter(|k| !k.is_empty()) {
            opts = opts.with_api_key(key);
        }

        // `--path-prefix` flag (or `BOXLITE_REST_PATH_PREFIX`, both filled by
        // clap into `self.path_prefix`) overrides the profile's routing slot.
        // Leaving it alone when the flag is unset means the profile's value
        // wins; if neither is set the URL builder skips the segment entirely
        // (`/v1/boxes/...`, the empty-prefix single-tenant shape).
        if let Some(path_prefix) = self.path_prefix.as_ref().filter(|s| !s.is_empty()) {
            opts.path_prefix = Some(path_prefix.clone());
        }

        Some(opts)
    }
}

// ============================================================================
// PROCESS FLAGS
// ============================================================================

#[derive(Args, Debug, Clone)]
pub struct ProcessFlags {
    /// Keep STDIN open even if not attached
    #[arg(short, long, conflicts_with = "detach")]
    pub interactive: bool,

    /// Allocate a pseudo-TTY (stdout and stderr are merged in TTY mode)
    #[arg(short, long)]
    pub tty: bool,

    /// Set environment variables
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,

    /// Working directory inside the box
    #[arg(short = 'w', long = "workdir")]
    pub workdir: Option<String>,

    /// User to run the command as (format: <name|uid>[:<group|gid>])
    #[arg(short = 'u', long = "user")]
    pub user: Option<String>,
}

impl ProcessFlags {
    /// Apply process configuration to BoxOptions
    pub fn apply_to(&self, opts: &mut BoxOptions) -> anyhow::Result<()> {
        self.apply_to_with_lookup(opts, |k| std::env::var(k).ok())
    }

    /// Internal helper for dependency injection of environment variables
    fn apply_to_with_lookup<F>(&self, opts: &mut BoxOptions, lookup: F) -> anyhow::Result<()>
    where
        F: Fn(&str) -> Option<String>,
    {
        opts.working_dir = self.workdir.clone();
        apply_env_vars_with_lookup(&self.env, opts, lookup);
        if let Some(ref user) = self.user {
            opts.user = Some(user.clone());
        }
        // `-t` is a property of the container's init, which COMMAND now is, so
        // it has to be decided here at create time rather than at attach.
        opts.tty = self.tty;
        Ok(())
    }

    /// Validate process flags
    pub fn validate(&self, detach: bool) -> anyhow::Result<()> {
        // Check TTY mode only in non-detach mode
        if !detach && self.tty && !std::io::stdin().is_terminal() {
            anyhow::bail!("the input device is not a TTY.");
        }

        Ok(())
    }

    /// Configures a BoxCommand with process flags (env, workdir, tty)
    pub fn configure_command(&self, mut cmd: BoxCommand) -> BoxCommand {
        for (key, value) in resolve_env_vars_with_lookup(&self.env, |key| std::env::var(key).ok()) {
            cmd = cmd.env(key, value);
        }

        if let Some(ref w) = self.workdir {
            cmd = cmd.working_dir(w);
        }

        if self.tty {
            cmd = cmd.tty(true);
        }

        if let Some(ref user) = self.user {
            cmd = cmd.user(user);
        }

        cmd
    }
}

// ============================================================================
// CAPABILITY FLAGS
// ============================================================================

#[derive(Args, Debug, Clone, Default)]
pub struct CapabilityFlags {
    /// Add a Linux capability to the container (repeatable; `ALL` is supported)
    #[arg(long = "cap-add", value_name = "CAPABILITY")]
    pub cap_add: Vec<String>,

    /// Drop a Linux capability from the container (repeatable; `ALL` is supported)
    #[arg(long = "cap-drop", value_name = "CAPABILITY")]
    pub cap_drop: Vec<String>,
}

impl CapabilityFlags {
    pub fn apply_to(&self, opts: &mut BoxOptions) {
        if self.cap_add.is_empty() && self.cap_drop.is_empty() {
            return;
        }
        opts.advanced
            .set_capabilities(Some(ContainerCapabilities {
                add: self.cap_add.clone(),
                drop: self.cap_drop.clone(),
            }))
            .expect("apply_to runs before options are resolved");
    }
}

// ============================================================================
// RESOURCE FLAGS
// ============================================================================

#[derive(Args, Debug, Clone)]
pub struct ResourceFlags {
    /// Number of CPUs
    #[arg(long)]
    pub cpus: Option<u32>,

    /// Memory limit (in MiB)
    #[arg(long)]
    pub memory: Option<u32>,

    /// Container rootfs disk size (in GB). The COW overlay is sparse —
    /// actual on-disk usage grows as the workload writes. The virtual
    /// size is `max(this, base image size)`; smaller values are ignored.
    /// Default (unset) sizes the overlay to exactly the base image,
    /// leaving no headroom — set this for workloads that write
    /// significant data (in-box `docker pull`, `apt install`, `npm
    /// install`, build caches, etc.).
    #[arg(long = "disk-size", value_name = "GB")]
    pub disk_size_gb: Option<u64>,
}

impl ResourceFlags {
    pub fn apply_to(&self, opts: &mut BoxOptions) {
        if let Some(cpus) = self.cpus {
            if cpus > 255 {
                tracing::warn!("CPU limit capped at 255 (requested {})", cpus);
            }
            opts.cpus = Some(cpus.min(255) as u8);
        }
        if let Some(mem) = self.memory {
            opts.memory_mib = Some(mem);
        }
        if let Some(gb) = self.disk_size_gb {
            opts.disk_size_gb = Some(gb);
        }
    }
}

// ============================================================================
// KERNEL FLAGS
// ============================================================================

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum KernelFormatArg {
    Auto,
    Raw,
    Elf,
    PeGz,
    ImageBz2,
    ImageGz,
    ImageZstd,
}

impl From<KernelFormatArg> for KernelFormat {
    fn from(format: KernelFormatArg) -> Self {
        match format {
            KernelFormatArg::Auto => Self::Auto,
            KernelFormatArg::Raw => Self::Raw,
            KernelFormatArg::Elf => Self::Elf,
            KernelFormatArg::PeGz => Self::PeGz,
            KernelFormatArg::ImageBz2 => Self::ImageBz2,
            KernelFormatArg::ImageGz => Self::ImageGz,
            KernelFormatArg::ImageZstd => Self::ImageZstd,
        }
    }
}

// Direct Linux boot options. Companion flags are valid only with `--kernel`.
#[derive(Args, Debug, Clone, Default)]
pub struct KernelFlags {
    /// Boot with this Linux kernel image instead of BoxLite's bundled kernel.
    #[arg(long, value_name = "PATH", hide = true)]
    pub kernel: Option<PathBuf>,

    /// Kernel image format. Auto-detected by default.
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        requires = "kernel",
        hide = true
    )]
    pub kernel_format: Option<KernelFormatArg>,

    /// Initial ramdisk to load with the custom kernel.
    #[arg(long, value_name = "PATH", requires = "kernel", hide = true)]
    pub initramfs: Option<PathBuf>,

    /// Replace libkrun's default Linux kernel command line.
    #[arg(
        long = "kernel-args",
        value_name = "ARGS",
        requires = "kernel",
        hide = true
    )]
    pub kernel_args: Option<String>,
}

impl KernelFlags {
    pub fn require_enabled(&self, features: &ExperimentalFeatures) -> boxlite::BoxliteResult<()> {
        if self.kernel.is_none() {
            return Ok(());
        }

        features.require(ExperimentalFeature::CustomKernel)
    }

    pub fn apply_to(&self, opts: &mut BoxOptions) {
        let Some(path) = &self.kernel else {
            return;
        };
        configure(
            opts,
            KernelOptions {
                path: path.clone(),
                format: self.kernel_format.map(Into::into).unwrap_or_default(),
                initramfs: self.initramfs.clone(),
                command_line: self.kernel_args.clone(),
            },
        );
    }
}

// ============================================================================
// NETWORK FLAGS
// ============================================================================

#[derive(Args, Debug, Clone)]
pub struct NetworkFlags {
    /// Network mode: "enabled" (default — full or allow-listed egress) or
    /// "disabled" (no interface at all; gvproxy is not started and the guest
    /// has no eth0).
    #[arg(long = "network", value_name = "MODE")]
    pub network: Option<String>,

    /// Restrict TCP and UDP egress to the listed hosts/IPs (repeatable);
    /// everything else is DNS-sinkholed and dropped. Implies network=enabled.
    /// Patterns: exact host, "*.example.com", IP, or CIDR. Hostname rules need
    /// TLS SNI / HTTP Host inspection, so a hostname-only list denies all UDP;
    /// add the IP or CIDR to keep UDP open. Incompatible with `--network
    /// disabled`.
    #[arg(long = "allow-net", value_name = "HOST")]
    pub allow_net: Vec<String>,

    /// Inbound mode: "enabled" (default — services the box exposes are
    /// publicly reachable) or "disabled" (private, unreachable from outside
    /// the box).
    #[arg(long = "inbound", value_name = "MODE")]
    pub inbound: Option<String>,

    /// Cap what the box sends, in kilobits/sec (guest to internet). Unset or 0
    /// leaves it uncapped. Local runtime only. Verified on Linux; on macOS the
    /// guest link is a datagram socket whose sender behaviour under
    /// backpressure is unverified, so this may drop frames instead of slowing
    /// the guest down.
    #[arg(long = "net-tx-kbps", value_name = "KBPS")]
    pub net_tx_kbps: Option<u64>,

    /// Cap what reaches the box, in kilobits/sec (internet to guest). Unset or
    /// 0 leaves it uncapped. Local runtime only. Paced the same way on Linux
    /// and macOS.
    #[arg(long = "net-rx-kbps", value_name = "KBPS")]
    pub net_rx_kbps: Option<u64>,
}

impl NetworkFlags {
    pub fn apply_to(&self, opts: &mut BoxOptions) -> anyhow::Result<()> {
        // Bandwidth is independent of mode and allowlist, so it is assigned
        // before the early return below. Folding it into that condition instead
        // would mean a lone --net-tx-kbps is silently dropped the day someone
        // adds another flag and forgets to extend the guard.
        opts.net_bandwidth = NetBandwidth {
            tx_kbps: self.net_tx_kbps,
            rx_kbps: self.net_rx_kbps,
        };

        // Leave BoxOptions::default() (outbound Enabled/full access, inbound
        // Enabled/public) untouched when no flag is given, so a bare `run`
        // behaves as before.
        if self.network.is_none() && self.allow_net.is_empty() && self.inbound.is_none() {
            return Ok(());
        }
        let mode = match self.network.as_deref() {
            Some(value) => value.parse::<NetworkMode>()?,
            None => NetworkMode::Enabled,
        };
        let inbound_mode = match self.inbound.as_deref() {
            Some(value) => value.parse::<NetworkMode>()?,
            None => NetworkMode::Enabled,
        };
        opts.network = NetworkSpec::try_from(OutboundNetworkConfig {
            mode,
            allow_net: self.allow_net.clone(),
        })?;
        opts.inbound_network = NetworkSpec::try_from(InboundNetworkConfig {
            mode: inbound_mode,
            allow_net: Vec::new(),
        })?;
        Ok(())
    }
}

// ============================================================================
// PUBLISH (PORT) FLAGS
// ============================================================================

#[derive(Args, Debug, Clone)]
pub struct PublishFlags {
    /// Publish a TCP box port locally (boxPort uses an automatic host port)
    #[arg(short = 'p', long = "publish", value_name = "PORT")]
    pub publish: Vec<String>,
}

impl PublishFlags {
    pub fn apply_to(&self, opts: &mut BoxOptions) -> anyhow::Result<()> {
        for s in &self.publish {
            let spec = parse_publish_spec(s)?;
            opts.ports.push(spec);
        }
        Ok(())
    }
}

/// Parse a single publish spec: `[hostPort:]boxPort[/tcp]`.
/// - `boxPort` → host_port=None, guest_port=boxPort
/// - `hostPort:boxPort` → host_port=Some(hostPort), guest_port=boxPort
fn parse_publish_spec(s: &str) -> anyhow::Result<PortSpec> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty port spec");
    }
    let (rest, protocol) = match s.split_once('/') {
        Some((r, proto)) => {
            let p = if proto.eq_ignore_ascii_case("tcp") {
                PortProtocol::Tcp
            } else if proto.eq_ignore_ascii_case("udp") {
                anyhow::bail!("UDP port forwarding is not implemented; use TCP")
            } else {
                anyhow::bail!("invalid protocol {:?}; use tcp", proto)
            };
            (r.trim(), p)
        }
        None => (s, PortProtocol::Tcp),
    };
    let parts: Vec<&str> = rest.splitn(2, ':').map(str::trim).collect();
    let (host_port, guest_port) = match parts.as_slice() {
        [guest] => {
            let g = parse_port(guest)?;
            (None, g)
        }
        [host, guest] => {
            let h = parse_port(host)?;
            let g = parse_port(guest)?;
            (Some(h), g)
        }
        _ => anyhow::bail!(
            "invalid port spec {:?}; use hostPort:boxPort or boxPort[/tcp]",
            s
        ),
    };
    Ok(PortSpec {
        host_port,
        guest_port,
        protocol,
        host_ip: None,
    })
}

fn parse_port(s: &str) -> anyhow::Result<u16> {
    let n: u16 = s
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid port number {:?}", s))?;
    if n == 0 {
        anyhow::bail!("port must be 1-65535");
    }
    Ok(n)
}

// ============================================================================
// VOLUME FLAGS
// ============================================================================

#[derive(Args, Debug, Clone)]
pub struct VolumeFlags {
    /// Mount a volume: VOLUME:BOX_PATH for a managed volume, HOST_PATH:BOX_PATH[:options]
    /// for a host bind (host paths start with `/`, `./`, `~` or a drive letter), or
    /// BOX_PATH[:options] for an anonymous volume
    #[arg(short = 'v', long = "volume", value_name = "VOLUME")]
    pub volume: Vec<String>,
}

/// Resolve base directory for anonymous volumes: explicit home, or BOXLITE_HOME, or ~/.boxlite, or temp dir.
fn anonymous_volume_base(home: Option<&std::path::Path>) -> std::path::PathBuf {
    home.map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("BOXLITE_HOME")
                .ok()
                .map(std::path::PathBuf::from)
        })
        .or_else(|| {
            dirs::home_dir().map(|mut p| {
                p.push(".boxlite");
                p
            })
        })
        .unwrap_or_else(std::env::temp_dir)
}

/// Make a host bind path absolute.
///
/// `volumespec` classified this as a path without touching the filesystem, so a
/// relative one is resolved here — the same split Docker uses, where the client
/// absolutizes and only the resolved path travels on.
fn resolve_host_path(path: String) -> anyhow::Result<String> {
    // A Windows path is absolute even where `Path::is_relative` says otherwise:
    // on Unix `C:\data` has no leading `/`, so without this it would be
    // canonicalized against the working directory and fail.
    if !std::path::Path::new(&path).is_relative()
        || crate::volumespec::is_windows_drive_prefix(&path)
    {
        return Ok(path);
    }

    let absolute = std::fs::canonicalize(&path)
        .map_err(|e| anyhow::anyhow!("volume host path {:?}: {}", path, e))?;
    Ok(absolute.to_string_lossy().into_owned())
}

impl VolumeFlags {
    /// Apply volume flags to options. Pass `home` for anonymous volume storage (e.g. from GlobalFlags).
    pub fn apply_to(
        &self,
        opts: &mut BoxOptions,
        home: Option<&std::path::Path>,
    ) -> anyhow::Result<()> {
        let base = anonymous_volume_base(home);
        for value in self.volume.iter() {
            let mount = crate::volumespec::parse(value)?;

            let spec = match mount.origin {
                // Held as written; the server resolves an id or a name.
                crate::volumespec::MountOrigin::ManagedVolume(volume) => {
                    // Neither the server nor the REST client accepts one yet;
                    // saying so here beats a downgrade the caller never sees.
                    if mount.read_only {
                        anyhow::bail!(
                            "read-only managed volumes are not supported yet; \
                             mount {volume:?} read-write"
                        );
                    }
                    VolumeSpec::managed_volume(volume, mount.guest_path)
                }

                crate::volumespec::MountOrigin::BindMount(path) => {
                    VolumeSpec::bind_mount(resolve_host_path(path)?, mount.guest_path)
                }

                crate::volumespec::MountOrigin::Anonymous => {
                    // Random id for the directory name (same approach as Podman:
                    // cryptographically random to avoid collisions under any load).
                    let unique = ulid::Ulid::new().to_string();
                    let dir = base.join("volumes").join("anonymous").join(unique);
                    std::fs::create_dir_all(&dir).map_err(|e| {
                        anyhow::anyhow!("failed to create anonymous volume dir {:?}: {}", dir, e)
                    })?;
                    VolumeSpec::bind_mount(dir.to_string_lossy().into_owned(), mount.guest_path)
                }
            };

            opts.volumes.push(VolumeSpec {
                read_only: mount.read_only,
                ..spec
            });
        }
        Ok(())
    }
}

// ============================================================================
// MANAGEMENT FLAGS
// ============================================================================

/// Parse a lifecycle duration: bare seconds, or one `s`/`m`/`h`/`d` suffix.
///
/// Bare digits stay seconds so the flags accept the same integers the wire
/// contract carries; the suffixes exist because a fortnight in seconds is not
/// a thing anyone should have to type.
fn parse_duration_seconds(raw: &str) -> Result<u32, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("duration must not be empty".to_string());
    }

    let (digits, multiplier) = match raw.as_bytes()[raw.len() - 1] {
        b's' => (&raw[..raw.len() - 1], 1),
        b'm' => (&raw[..raw.len() - 1], 60),
        b'h' => (&raw[..raw.len() - 1], 3_600),
        b'd' => (&raw[..raw.len() - 1], 86_400),
        _ => (raw, 1),
    };

    let value: u32 = digits.parse().map_err(|_| {
        format!("duration {raw:?} must be a whole number of seconds, or use s/m/h/d")
    })?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration {raw:?} is too large"))
}

#[derive(Args, Debug, Clone)]
pub struct ManagementFlags {
    /// Assign a name to the box
    #[arg(long)]
    pub name: Option<String>,

    /// Stop the box after this much inactivity; `0` disables.
    ///
    /// Seconds when bare, or a suffixed duration: `30s`, `15m`, `2h`, `7d`.
    /// Requires a server (`--url`, or a configured profile): the deadline is
    /// swept on a timer, which only `boxlite serve` and the cloud run. Against
    /// the embedded runtime a non-zero value is rejected.
    #[arg(long = "auto-stop", value_name = "DURATION", value_parser = parse_duration_seconds)]
    pub auto_stop: Option<u32>,

    /// Delete the box this long after it stops; `0` disables.
    ///
    /// Same grammar and the same server requirement as `--auto-stop`.
    #[arg(long = "auto-delete", value_name = "DURATION", value_parser = parse_duration_seconds)]
    pub auto_delete: Option<u32>,

    /// Refuse operations that would implicitly wake a stopped box.
    ///
    /// Without this, exec/files/attach against a stopped box start it first.
    /// A box that has never run is unaffected: its first boot is not a resume.
    /// Enforced by the server; the embedded runtime records the preference but
    /// has no request path to refuse.
    #[arg(long = "no-auto-resume")]
    pub no_auto_resume: bool,

    /// Sandbox security: `enable` (default) or `disable` (case-insensitive).
    /// Absent → the box uses `SecurityOptions::default()` = enable, the
    /// fully-isolated profile. Use `--security=disable` to turn the sandbox
    /// off (master switch + all sub-protections) when debugging.
    #[arg(long, env = "BOXLITE_SECURITY")]
    pub security: Option<String>,

    /// Require nested virtualization for workloads in the box.
    /// Starting the box fails if the host cannot provide it.
    #[arg(long = "nested-virtualization", hide = true)]
    pub nested_virtualization: bool,
}

impl ManagementFlags {
    pub fn require_enabled(&self, features: &ExperimentalFeatures) -> boxlite::BoxliteResult<()> {
        if !self.nested_virtualization {
            return Ok(());
        }

        features.require(ExperimentalFeature::NestedVirtualization)
    }

    /// Refuse a deadline nothing will act on.
    ///
    /// Both deadlines need a sweeper, which only a server runs. Against the
    /// embedded runtime `--auto-stop` is refused by the engine anyway
    /// (`reject_local_unsupported_options`), and `--auto-delete` is worse than
    /// refused: `removes_on_stop()` is `effective_auto_delete() > 0`, so it
    /// would silently delete the box at its stop — the `--rm` behaviour the
    /// parser declares mutually exclusive with it. Failing here says so, and
    /// says it the same way for both flags.
    pub fn require_sweeper(&self, targets_rest: bool) -> anyhow::Result<()> {
        if targets_rest {
            return Ok(());
        }
        for (flag, seconds) in [
            ("--auto-stop", self.auto_stop),
            ("--auto-delete", self.auto_delete),
        ] {
            if seconds.is_some_and(|seconds| seconds > 0) {
                anyhow::bail!(
                    "{flag} needs a server to enforce it — point at one with --url \
                     (or a configured profile). Use `boxlite run --rm` when \
                     immediate local cleanup is required."
                );
            }
        }
        Ok(())
    }

    pub fn apply_to(&self, opts: &mut BoxOptions) -> anyhow::Result<()> {
        // The CLI default (like `docker run`) is to keep the box. `run --rm`
        // overrides this sentinel at its command boundary; `create` does not
        // expose remove-on-stop because it always creates a detached box.
        opts.auto_delete = Some(0);

        // An explicit deadline replaces that sentinel. It has to be written
        // after it, not before: the line above is unconditional, so setting
        // these first would have `--auto-delete 3600` overwritten by the `0`
        // that means "no deadline". `--rm` and `--auto-delete` are mutually
        // exclusive at the parser, so only one of them is ever meaningful.
        if let Some(seconds) = self.auto_delete {
            opts.auto_delete = Some(seconds);
        }
        if let Some(seconds) = self.auto_stop {
            opts.auto_stop = Some(seconds);
        }
        if self.no_auto_resume {
            opts.auto_resume = Some(false);
        }
        if let Some(ref preset) = self.security {
            // Bubble the typo'd-preset error all the way back to the
            // CLI exit so the operator sees the offending value.
            opts.advanced.security =
                boxlite::SecurityOptions::from_preset(preset).map_err(anyhow::Error::from)?;
        }
        if self.nested_virtualization {
            opts.advanced.nested_virtualization = true;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boxlite::runtime::options::NetworkSpec;
    use std::fs;

    fn parse_projected(args: &[&str]) -> Cli {
        try_parse_from(args).expect("parse")
    }

    #[test]
    fn command_tree_owns_after_command_scope_rejection() {
        for args in [
            &["boxlite", "serve", "--url", "https://irrelevant.test"][..],
            &["boxlite", "ls", "--registry", "registry.example.test"][..],
            &[
                "boxlite",
                "auth",
                "logout",
                "--url",
                "https://irrelevant.test",
            ][..],
            &[
                "boxlite",
                "volume",
                "list",
                "--config",
                "/tmp/irrelevant.json",
            ][..],
            &["boxlite", "completion", "bash", "--debug"][..],
        ] {
            let error = command()
                .try_get_matches_from(args)
                .expect_err("the command tree must reject an out-of-scope option");
            assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{args:?}");
        }
    }

    #[test]
    fn scoped_command_tree_is_valid_and_preserves_root_and_leaf_flag_positions() {
        command().debug_assert();

        for args in [
            ["boxlite", "--url", "https://example.test", "run", "alpine"],
            ["boxlite", "run", "--url", "https://example.test", "alpine"],
        ] {
            let cli = parse_projected(&args);
            assert_eq!(cli.global.url.as_deref(), Some("https://example.test"));
            assert!(matches!(cli.command, Commands::Run(_)));
        }

        for args in [
            &["boxlite", "run", "--path-prefix", "", "alpine"][..],
            &["boxlite", "run", "--profile", "", "alpine"][..],
            &["boxlite", "run", "--registry", "", "alpine"][..],
            &["boxlite", "run", "--config", "", "alpine"][..],
        ] {
            let error = try_parse_from(args).expect_err("empty global value must be rejected");
            assert_eq!(error.kind(), ErrorKind::ValueValidation, "{args:?}");
        }

        for args in [
            ["boxlite", "--url", "https://example.test", "auth", "login"],
            ["boxlite", "auth", "login", "--url", "https://example.test"],
        ] {
            let cli = parse_projected(&args);
            let Commands::Auth(auth) = cli.command else {
                panic!("expected auth");
            };
            let crate::commands::auth::AuthCommand::Login(login) = auth.command else {
                panic!("expected login");
            };
            assert_eq!(login.url.as_deref(), Some("https://example.test"));
        }

        for args in [
            &["boxlite", "--url", "https://example.test", "auth", "status"][..],
            &["boxlite", "auth", "status", "--url", "https://example.test"][..],
        ] {
            let cli = parse_projected(args);
            assert_eq!(cli.global.url.as_deref(), Some("https://example.test"));
        }

        let error = try_parse_from(["boxlite", "auth", "--url", "https://example.test", "status"])
            .expect_err("leaf-only --url must follow the auth leaf");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn root_and_leaf_repeatable_global_values_are_both_preserved() {
        let cli = parse_projected(&[
            "boxlite",
            "--registry",
            "root.example.test",
            "run",
            "--registry",
            "leaf.example.test",
            "alpine",
        ]);

        assert_eq!(
            cli.global.registry,
            ["root.example.test", "leaf.example.test"]
        );
    }

    #[test]
    fn auth_login_rejects_an_explicit_empty_leaf_url() {
        let error = try_parse_from(["boxlite", "auth", "login", "--url", ""])
            .expect_err("an explicit empty URL must not fall through to another target");

        assert_eq!(error.kind(), ErrorKind::InvalidValue);
    }

    fn global_option_tokens(id: &str) -> Vec<&'static str> {
        match id {
            "debug" => vec!["--debug"],
            "home" => vec!["--home", "/tmp/boxlite-strict-options"],
            "registry" => vec!["--registry", "registry.example.test"],
            "config" => vec!["--config", "/tmp/boxlite-config.json"],
            "url" => vec!["--url", "https://example.test"],
            "profile" => vec!["--profile", "test-profile"],
            "path_prefix" => vec!["--path-prefix", "tenant"],
            other => panic!("missing command-line spelling for {other}"),
        }
    }

    const REGISTRY: &[&str] = &["registry"];
    const LOCAL_WITHOUT_REGISTRY: &[&str] = &["registry", "url", "profile", "path_prefix"];
    const LOCAL_WITH_REGISTRY: &[&str] = &["url", "profile", "path_prefix"];
    const AUTH_WITH_URL: &[&str] = &["registry", "config", "path_prefix"];
    const AUTH_WITHOUT_URL: &[&str] = &["registry", "config", "url", "path_prefix"];
    const VOLUME: &[&str] = &["registry", "config"];
    const ALL_GLOBALS: &[&str] = &[
        "debug",
        "home",
        "registry",
        "config",
        "url",
        "profile",
        "path_prefix",
    ];
    const IRRELEVANT_GLOBAL_CASES: &[(&[&str], usize, &[&str])] = &[
        (&["boxlite", "exec", "box", "--", "true"], 1, REGISTRY),
        (&["boxlite", "list"], 1, REGISTRY),
        (&["boxlite", "ls"], 1, REGISTRY),
        (&["boxlite", "ps"], 1, REGISTRY),
        (&["boxlite", "rm", "box"], 1, REGISTRY),
        (&["boxlite", "start", "box"], 1, REGISTRY),
        (&["boxlite", "stop", "box"], 1, REGISTRY),
        (&["boxlite", "restart", "box"], 1, REGISTRY),
        (&["boxlite", "inspect", "box"], 1, REGISTRY),
        (&["boxlite", "cp", "box:/src", "/tmp/dst"], 1, REGISTRY),
        (&["boxlite", "stats", "box"], 1, REGISTRY),
        (
            &["boxlite", "network", "tunnel", "box", "3000"],
            2,
            REGISTRY,
        ),
        (&["boxlite", "images"], 1, LOCAL_WITHOUT_REGISTRY),
        (&["boxlite", "info"], 1, LOCAL_WITHOUT_REGISTRY),
        (&["boxlite", "logs", "box"], 1, LOCAL_WITHOUT_REGISTRY),
        (&["boxlite", "pull", "alpine"], 1, LOCAL_WITH_REGISTRY),
        (&["boxlite", "serve"], 1, LOCAL_WITH_REGISTRY),
        (&["boxlite", "auth", "login"], 2, AUTH_WITH_URL),
        (&["boxlite", "auth", "status"], 2, AUTH_WITH_URL),
        (&["boxlite", "auth", "whoami"], 2, AUTH_WITH_URL),
        (&["boxlite", "auth", "logout"], 2, AUTH_WITHOUT_URL),
        (&["boxlite", "volume", "create"], 2, VOLUME),
        (&["boxlite", "volume", "ls"], 2, VOLUME),
        (&["boxlite", "volume", "list"], 2, VOLUME),
        (&["boxlite", "volume", "get", "volume"], 2, VOLUME),
        (&["boxlite", "volume", "inspect", "volume"], 2, VOLUME),
        (&["boxlite", "volume", "rm", "volume"], 2, VOLUME),
        (&["boxlite", "volume", "delete", "volume"], 2, VOLUME),
        (&["boxlite", "completion", "bash"], 1, ALL_GLOBALS),
    ];

    fn assert_irrelevant_global_rejected(base: &[&str], command_depth: usize, id: &str) {
        let tokens = global_option_tokens(id);
        let mut positions = vec![1, command_depth + 1];
        if command_depth > 1 {
            positions.push(2);
        }
        positions.push(
            base.iter()
                .position(|token| *token == "--")
                .unwrap_or(base.len()),
        );
        positions.sort_unstable();
        positions.dedup();

        for position in positions {
            let mut args = base.to_vec();
            args.splice(position..position, tokens.iter().copied());
            let error = try_parse_from(&args).expect_err("irrelevant option must be rejected");
            assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{args:?}");
            assert!(
                error.to_string().contains(tokens[0]),
                "error must identify {}: {error}",
                tokens[0]
            );
        }
    }

    #[test]
    fn every_command_path_rejects_irrelevant_global_options() {
        for (base, command_depth, hidden) in IRRELEVANT_GLOBAL_CASES {
            for id in *hidden {
                assert_irrelevant_global_rejected(base, *command_depth, id);
            }
        }
    }

    #[test]
    fn command_tree_rejects_irrelevant_options_encountered_before_help() {
        for args in [
            &[
                "boxlite",
                "exec",
                "--registry",
                "registry.example.test",
                "--help",
            ][..],
            &[
                "boxlite",
                "auth",
                "logout",
                "--url=https://irrelevant.test",
                "--help",
            ][..],
            &[
                "boxlite",
                "volume",
                "list",
                "--config",
                "/tmp/irrelevant.json",
                "--help",
            ][..],
            &["boxlite", "completion", "--debug", "bash", "--help"][..],
        ] {
            let error = try_parse_from(args).expect_err("irrelevant option precedes help");
            assert_eq!(error.kind(), ErrorKind::UnknownArgument, "{args:?}");
        }
    }

    #[test]
    fn display_actions_remain_terminal() {
        for args in [
            &[
                "boxlite",
                "--help",
                "serve",
                "--url",
                "https://example.test",
            ][..],
            &[
                "boxlite",
                "-h",
                "completion",
                "--url",
                "https://example.test",
            ][..],
            &[
                "boxlite",
                "serve",
                "--help",
                "--url",
                "https://example.test",
            ][..],
            &[
                "boxlite",
                "auth",
                "logout",
                "--help",
                "--url",
                "https://example.test",
            ][..],
            &[
                "boxlite",
                "--registry",
                "registry.example.test",
                "exec",
                "--help",
            ][..],
            &["boxlite", "--url", "https://example.test", "help", "serve"][..],
        ] {
            let error = try_parse_from(args).expect_err("help exits through clap");
            assert_eq!(error.kind(), ErrorKind::DisplayHelp, "{args:?}");
        }

        let error = try_parse_from([
            "boxlite",
            "--registry",
            "registry.example.test",
            "--version",
        ])
        .expect_err("version exits through clap");
        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
    }

    #[test]
    fn completion_uses_the_same_global_capability_matrix() {
        let mut completion = completion_command();
        completion.build();

        let serve = completion
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "serve")
            .unwrap();
        assert!(!serve.get_arguments().any(|arg| arg.get_id() == "url"));

        let run = completion
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "run")
            .unwrap();
        assert!(run.get_arguments().any(|arg| arg.get_id() == "url"));
    }

    #[test]
    fn parser_rejects_options_that_cannot_affect_the_command() {
        for args in [
            &[
                "boxlite",
                "exec",
                "--entrypoint",
                "/bin/sh",
                "box",
                "--",
                "true",
            ][..],
            &["boxlite", "create", "--detach", "alpine"][..],
            &["boxlite", "create", "--rm", "alpine"][..],
            &["boxlite", "images", "--all"][..],
            &["boxlite", "run", "-i", "-d", "alpine"][..],
            &["boxlite", "exec", "-i", "-d", "box", "--", "true"][..],
            &["boxlite", "rm", "--all", "box"][..],
            &["boxlite", "inspect", "--latest", "box"][..],
            &["boxlite", "auth", "login", "--callback-port", "0"][..],
            &["boxlite", "list", "--quiet", "--format", "json"][..],
            &["boxlite", "images", "--quiet", "--format", "json"][..],
            &["boxlite", "volume", "ls", "--quiet", "--format", "json"][..],
            &["boxlite", "serve", "--api-key", ""][..],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "accepted {args:?}");
        }

        let cli = Cli::try_parse_from(["boxlite", "run", "--entrypoint", "/bin/sh", "alpine"])
            .expect("run owns --entrypoint");
        let Commands::Run(run) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(run.entrypoint.as_deref(), Some("/bin/sh"));

        let cli = Cli::try_parse_from(["boxlite", "cp", "--no-include-parent", "box:/src", "/dst"])
            .expect("inverse parent option parses");
        let Commands::Cp(cp) = cli.command else {
            panic!("expected cp");
        };
        assert!(cp.no_include_parent);
    }
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn test_apply_env_vars_with_lookup() {
        let mut opts = BoxOptions::default();
        let current_env = vec![
            "TEST_VAR=test_value".to_string(),
            "TEST_HOST_VAR".to_string(),
            "NON_EXISTENT_VAR".to_string(),
        ];

        apply_env_vars_with_lookup(&current_env, &mut opts, |k| {
            if k == "TEST_HOST_VAR" {
                Some("host_value".to_string())
            } else {
                None
            }
        });

        assert!(
            opts.env
                .contains(&("TEST_VAR".to_string(), "test_value".to_string()))
        );

        assert!(
            opts.env
                .contains(&("TEST_HOST_VAR".to_string(), "host_value".to_string()))
        );

        assert!(!opts.env.iter().any(|(k, _)| k == "NON_EXISTENT_VAR"));
    }

    #[test]
    fn resolve_runtime_options_prepends_cli_registries_to_config() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
                "image_registries": [
                    {
                        "host": "registry.local:5000",
                        "transport": "http",
                        "search": true
                    }
                ]
            }"#,
        )
        .unwrap();

        let flags = GlobalFlags {
            debug: false,
            home: Some(temp_dir.path().join("home")),
            registry: vec!["cli.registry.local".to_string()],
            config: Some(config_path.display().to_string()),
            url: None,
            profile: None,
            path_prefix: None,
            experimental_features: ExperimentalFeatures::default(),
        };

        let options = flags.resolve_runtime_options().unwrap();

        assert_eq!(options.home_dir, temp_dir.path().join("home"));
        assert_eq!(
            options.image_registries,
            vec![
                ImageRegistry::https("cli.registry.local").with_search(true),
                ImageRegistry::http("registry.local:5000").with_search(true),
            ]
        );
    }

    fn rest_flags(
        url: Option<&str>,
        profile: Option<&str>,
        path_prefix: Option<&str>,
    ) -> GlobalFlags {
        GlobalFlags {
            debug: false,
            home: None,
            registry: vec![],
            config: None,
            url: url.map(str::to_string),
            profile: profile.map(str::to_string),
            path_prefix: path_prefix.map(str::to_string),
            experimental_features: ExperimentalFeatures::default(),
        }
    }

    fn api_key_profile(path_prefix: Option<&str>) -> crate::credentials::Profile {
        crate::credentials::Profile {
            url: "https://api.example.com".to_string(),
            path_prefix: path_prefix.map(str::to_string),
            auth_method: crate::credentials::AuthMethod::ApiKey,
            api_key: Some(secrecy::SecretString::from("profile-bearer".to_string())),
            ..Default::default()
        }
    }

    #[test]
    fn api_key_env_preserves_profile_path_prefix() {
        // Regression: an ambient BOXLITE_API_KEY must override only the bearer,
        // not silently discard the selected profile's routing slot — dropping
        // it made the URL builder emit the prefix-less `/v1/boxes` shape, which
        // a multi-tenant server rejects with 404.
        let flags = rest_flags(None, Some("p1"), None);
        let opts = flags
            .resolve_rest_options(
                Some(api_key_profile(Some("acme"))),
                Some("env-key".to_string()),
            )
            .expect("REST options resolved");
        assert_eq!(
            opts.path_prefix.as_deref(),
            Some("acme"),
            "profile routing slot must survive an ambient BOXLITE_API_KEY"
        );
    }

    #[tokio::test]
    async fn api_key_env_overrides_profile_bearer_but_keeps_prefix() {
        // Confirmed precedence: env key wins for the bearer, profile prefix stays.
        let flags = rest_flags(None, Some("p1"), None);
        let opts = flags
            .resolve_rest_options(
                Some(api_key_profile(Some("acme"))),
                Some("env-key".to_string()),
            )
            .expect("REST options resolved");

        let token = opts
            .credential
            .expect("credential set")
            .get_token()
            .await
            .expect("token")
            .token;
        assert_eq!(
            token, "env-key",
            "BOXLITE_API_KEY overrides the profile bearer"
        );
        assert_eq!(
            opts.path_prefix.as_deref(),
            Some("acme"),
            "prefix preserved alongside the overridden bearer"
        );
    }

    #[test]
    fn api_key_env_without_profile_has_no_prefix() {
        // No profile → no routing slot, even with a key (single-tenant shape).
        let flags = rest_flags(Some("https://api.example.com"), None, None);
        let opts = flags
            .resolve_rest_options(None, Some("env-key".to_string()))
            .expect("REST options resolved");
        assert!(opts.path_prefix.is_none());
    }

    #[test]
    fn test_resource_flags_cpu_cap() {
        let flags = ResourceFlags {
            cpus: Some(1000),
            memory: None,
            disk_size_gb: None,
        };

        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts);

        assert_eq!(opts.cpus, Some(255));
    }

    #[test]
    fn test_resource_flags_disk_size_plumbed() {
        // --disk-size <GB> must reach BoxOptions.disk_size_gb verbatim so the
        // COW overlay in container_rootfs::create_cow_disk picks up
        // max(user_size, base_image_size). A regression that drops this
        // flag would leave agent-workflow tests at base-image size and
        // they'd silently ENOSPC mid-`docker pull`.
        let flags = ResourceFlags {
            cpus: None,
            memory: None,
            disk_size_gb: Some(10),
        };

        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts);

        assert_eq!(opts.disk_size_gb, Some(10));
    }

    #[test]
    fn test_resource_flags_disk_size_default_unset() {
        // No --disk-size on the command line means BoxOptions.disk_size_gb
        // stays None — container_rootfs::create_cow_disk's `if let Some`
        // branch is skipped and the COW disk is exactly the base image
        // size. This is the documented default; the test pins it so a
        // refactor that injects a fallback (`unwrap_or(N)`) would fail.
        let flags = ResourceFlags {
            cpus: None,
            memory: None,
            disk_size_gb: None,
        };

        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts);

        assert_eq!(opts.disk_size_gb, None);
    }

    #[test]
    fn custom_kernel_flags_are_applied_as_one_boot_configuration() {
        let flags = KernelFlags {
            kernel: Some(PathBuf::from("/tmp/vmlinux")),
            kernel_format: Some(KernelFormatArg::Elf),
            initramfs: Some(PathBuf::from("/tmp/initramfs.img")),
            kernel_args: Some("console=ttyS0 panic=-1".to_string()),
        };

        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts);

        let kernel = opts.advanced.kernel.expect("custom kernel configuration");
        assert_eq!(kernel.path, PathBuf::from("/tmp/vmlinux"));
        assert_eq!(kernel.format, KernelFormat::Elf);
        assert_eq!(kernel.initramfs, Some(PathBuf::from("/tmp/initramfs.img")));
        assert_eq!(
            kernel.command_line.as_deref(),
            Some("console=ttyS0 panic=-1")
        );
    }

    #[test]
    fn custom_kernel_gate_uses_explicit_feature_state() {
        let flags = KernelFlags {
            kernel: Some(PathBuf::from("/tmp/vmlinux")),
            ..Default::default()
        };

        let error = flags
            .require_enabled(&ExperimentalFeatures::default())
            .expect_err("custom kernel must be disabled by default");
        assert!(
            error
                .to_string()
                .contains("ExperimentalFeature::CustomKernel")
        );

        let enabled = ExperimentalFeatures::parse("custom-kernel").unwrap();
        flags.require_enabled(&enabled).unwrap();
    }

    #[test]
    fn nested_virtualization_gate_uses_explicit_feature_state() {
        let flags = ManagementFlags {
            nested_virtualization: true,
            auto_stop: None,
            auto_delete: None,
            no_auto_resume: false,
            name: None,
            security: None,
        };

        let error = flags
            .require_enabled(&ExperimentalFeatures::default())
            .expect_err("nested virtualization must be disabled by default");
        assert!(
            error
                .to_string()
                .contains("ExperimentalFeature::NestedVirtualization")
        );

        let enabled = ExperimentalFeatures::parse("nested-virtualization").unwrap();
        flags.require_enabled(&enabled).unwrap();
    }

    #[test]
    fn experimental_flags_are_hidden_from_help() {
        for command in ["run", "create"] {
            let error = Cli::try_parse_from(["boxlite", command, "--help"]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);

            let help = error.to_string();
            for rc_flag in [
                "--kernel",
                "--kernel-format",
                "--initramfs",
                "--kernel-args",
                "--nested-virtualization",
            ] {
                assert!(
                    !help.contains(rc_flag),
                    "{rc_flag} leaked into {command} help:\n{help}"
                );
            }
            assert!(
                !help.contains("\nAdvanced boot options:\n"),
                "empty advanced boot section leaked into {command} help:\n{help}"
            );
        }
    }

    #[test]
    fn experimental_flags_are_hidden_from_completions() {
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let mut output = Vec::new();
            generate_completion(&shell, "boxlite", &mut output);
            let completion = String::from_utf8(output).unwrap();

            for name in [
                "kernel",
                "kernel-format",
                "initramfs",
                "kernel-args",
                "nested-virtualization",
            ] {
                let rc_flag = match shell {
                    Shell::Fish => format!("-l {name}"),
                    Shell::Bash | Shell::Zsh => format!("--{name}"),
                };
                assert!(
                    !completion.contains(&rc_flag),
                    "{rc_flag} leaked into {shell:?} completion"
                );
            }
            for name in ["rootfs", "cpus", "network"] {
                let stable_flag = match shell {
                    Shell::Fish => format!("-l {name}"),
                    Shell::Bash | Shell::Zsh => format!("--{name}"),
                };
                assert!(
                    completion.contains(&stable_flag),
                    "{stable_flag} missing from {shell:?} completion"
                );
            }
        }
    }

    #[test]
    fn kernel_companion_flags_require_kernel() {
        let error = Cli::try_parse_from([
            "boxlite",
            "run",
            "--initramfs",
            "/tmp/initramfs.img",
            "alpine",
        ])
        .unwrap_err();

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    fn network_flags(network: Option<&str>, allow_net: &[&str]) -> NetworkFlags {
        NetworkFlags {
            network: network.map(str::to_string),
            allow_net: allow_net.iter().map(|s| s.to_string()).collect(),
            inbound: None,
            net_tx_kbps: None,
            net_rx_kbps: None,
        }
    }

    /// apply_to returns early when no mode or allowlist flag is set. A bandwidth
    /// cap must survive that path, or `--net-tx-kbps` on its own is dropped with
    /// no error at all.
    #[test]
    fn bandwidth_flags_apply_without_any_other_network_flag() {
        let mut opts = BoxOptions::default();
        let mut flags = network_flags(None, &[]);
        flags.net_tx_kbps = Some(10_000);

        flags.apply_to(&mut opts).expect("apply_to");

        assert_eq!(opts.net_bandwidth.tx_kbps, Some(10_000));
        assert_eq!(opts.net_bandwidth.rx_kbps, None);
        // The early return still protects the untouched network defaults.
        assert!(matches!(
            opts.network,
            boxlite::NetworkSpec::Enabled { ref allow_net } if allow_net.is_empty()
        ));
    }

    #[test]
    fn bandwidth_flags_apply_alongside_an_allowlist() {
        let mut opts = BoxOptions::default();
        let mut flags = network_flags(None, &["example.com"]);
        flags.net_tx_kbps = Some(1_000);
        flags.net_rx_kbps = Some(2_000);

        flags.apply_to(&mut opts).expect("apply_to");

        assert_eq!(opts.net_bandwidth.tx_kbps, Some(1_000));
        assert_eq!(opts.net_bandwidth.rx_kbps, Some(2_000));
    }

    #[test]
    fn test_network_flags_default_left_untouched() {
        // Neither flag set => BoxOptions::default() network is preserved
        // (Enabled, empty allow_net), so a bare `run` keeps full access.
        let mut opts = BoxOptions::default();
        network_flags(None, &[])
            .apply_to(&mut opts)
            .expect("no-op apply");

        assert!(
            matches!(opts.network, NetworkSpec::Enabled { ref allow_net } if allow_net.is_empty())
        );
    }

    #[test]
    fn test_network_flags_disabled() {
        // --network disabled => NetworkSpec::Disabled (no eth0, gvproxy off).
        let mut opts = BoxOptions::default();
        network_flags(Some("disabled"), &[])
            .apply_to(&mut opts)
            .expect("disabled is valid");

        assert!(matches!(opts.network, NetworkSpec::Disabled));
    }

    #[test]
    fn test_network_flags_allow_net_implies_enabled() {
        // --allow-net without --network => Enabled with the egress allowlist,
        // matching the REST NetworkConfig{mode, allow_net} mapping.
        let mut opts = BoxOptions::default();
        network_flags(None, &["api.openai.com", "10.0.0.0/8"])
            .apply_to(&mut opts)
            .expect("allow-net implies enabled");

        match opts.network {
            NetworkSpec::Enabled { allow_net } => {
                assert_eq!(allow_net, vec!["api.openai.com", "10.0.0.0/8"]);
            }
            other => panic!("expected Enabled with allowlist, got {other:?}"),
        }
    }

    #[test]
    fn test_network_flags_disabled_with_allow_net_is_rejected() {
        // --network disabled + --allow-net is contradictory; the error comes
        // from NetworkSpec::try_from (single source of truth), not the CLI.
        let mut opts = BoxOptions::default();
        let err = network_flags(Some("disabled"), &["api.openai.com"])
            .apply_to(&mut opts)
            .expect_err("disabled + allow-net must error");

        assert!(err.to_string().contains("allow_net"));
    }

    #[test]
    fn test_network_flags_invalid_mode_is_rejected() {
        // Unknown mode strings surface NetworkMode::from_str's error rather
        // than silently defaulting to enabled.
        let mut opts = BoxOptions::default();
        let err = network_flags(Some("bridge"), &[])
            .apply_to(&mut opts)
            .expect_err("unknown mode must error");

        assert!(err.to_string().contains("network mode"));
    }

    #[test]
    fn test_network_flags_inbound_disabled_sets_private() {
        // --inbound disabled alone (no --network/--allow-net) still applies,
        // and leaves outbound at its Enabled/full-access default.
        let mut opts = BoxOptions::default();
        let mut flags = network_flags(None, &[]);
        flags.inbound = Some("disabled".to_string());
        flags.apply_to(&mut opts).expect("disabled is valid");

        assert!(matches!(opts.inbound_network, NetworkSpec::Disabled));
        assert!(
            matches!(opts.network, NetworkSpec::Enabled { ref allow_net } if allow_net.is_empty())
        );
    }

    #[test]
    fn cli_rejects_inbound_allow_net_flag() {
        // The flag is deliberately not exposed until inbound allowlist
        // enforcement exists — a flag that always errors would advertise a
        // feature that doesn't work.
        let err = Cli::try_parse_from([
            "boxlite",
            "run",
            "--inbound-allow-net",
            "10.0.0.0/8",
            "alpine:latest",
        ])
        .expect_err("unknown flag must fail parsing");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn cli_parses_run_with_inbound_flags() {
        let cli = Cli::try_parse_from(["boxlite", "run", "--inbound", "disabled", "alpine:latest"])
            .expect("parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected Run")
        };
        assert_eq!(args.network.inbound.as_deref(), Some("disabled"));
    }

    #[test]
    fn test_network_flags_invalid_inbound_mode_is_rejected() {
        let mut opts = BoxOptions::default();
        let mut flags = network_flags(None, &[]);
        flags.inbound = Some("bridge".to_string());
        let err = flags
            .apply_to(&mut opts)
            .expect_err("unknown inbound mode must error");

        assert!(err.to_string().contains("network mode"));
    }

    fn process_flags() -> ProcessFlags {
        ProcessFlags {
            interactive: false,
            tty: false,
            env: Vec::new(),
            workdir: None,
            user: None,
        }
    }

    /// `-t` has to reach `BoxOptions.tty`, because the terminal now belongs to
    /// the *container's* init rather than to an exec: nothing downstream can
    /// add it later. When this mapping was missing, `run -it` still parsed,
    /// still put the local terminal in raw mode, and still ran — just against a
    /// process on pipes, with no prompt and no job control.
    #[test]
    fn test_process_flags_tty_reaches_box_options() {
        let mut opts = BoxOptions::default();
        assert!(!opts.tty, "a box is not a terminal by default");

        let mut flags = process_flags();
        flags.tty = true;
        flags.apply_to(&mut opts).expect("tty apply");

        assert!(opts.tty, "-t must make the main command a terminal");
    }

    #[test]
    fn test_parse_publish_spec_host_box() {
        let spec = super::parse_publish_spec("18789:18789").unwrap();
        assert_eq!(spec.host_port, Some(18789));
        assert_eq!(spec.guest_port, 18789);
        assert!(matches!(spec.protocol, PortProtocol::Tcp));
    }

    #[test]
    fn test_parse_publish_spec_host_box_tcp() {
        let spec = super::parse_publish_spec("8080:80/tcp").unwrap();
        assert_eq!(spec.host_port, Some(8080));
        assert_eq!(spec.guest_port, 80);
        assert!(matches!(spec.protocol, PortProtocol::Tcp));
    }

    #[test]
    fn test_parse_publish_spec_box_only() {
        let spec = super::parse_publish_spec("80").unwrap();
        assert_eq!(spec.host_port, None);
        assert_eq!(spec.guest_port, 80);
    }

    #[test]
    fn test_parse_publish_spec_udp_is_rejected() {
        let err = super::parse_publish_spec("53:53/udp").unwrap_err();
        assert!(err.to_string().contains("UDP port forwarding"));
    }

    #[test]
    fn test_parse_publish_spec_invalid_protocol() {
        assert!(super::parse_publish_spec("80:80/sctp").is_err());
    }

    #[test]
    fn test_parse_publish_spec_invalid_port() {
        assert!(super::parse_publish_spec("0:80").is_err());
        assert!(super::parse_publish_spec("99999:80").is_err());
    }

    #[test]
    fn test_publish_flags_apply_to() {
        let flags = PublishFlags {
            publish: vec!["18789:18789".to_string(), "8080:80/tcp".to_string()],
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts).unwrap();
        assert_eq!(opts.ports.len(), 2);
        assert_eq!(opts.ports[0].host_port, Some(18789));
        assert_eq!(opts.ports[0].guest_port, 18789);
        assert_eq!(opts.ports[1].host_port, Some(8080));
        assert_eq!(opts.ports[1].guest_port, 80);
    }

    #[test]
    fn test_volume_flags_apply_to() {
        let flags = VolumeFlags {
            volume: vec![
                "/host/data:/guest/data".to_string(),
                "/readonly:/ro:ro".to_string(),
            ],
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts, None).unwrap();
        assert_eq!(opts.volumes.len(), 2);
        assert_eq!(opts.volumes[0].host_path, "/host/data");
        assert_eq!(opts.volumes[0].guest_path, "/guest/data");
        assert!(!opts.volumes[0].read_only);
        assert_eq!(opts.volumes[1].host_path, "/readonly");
        assert_eq!(opts.volumes[1].guest_path, "/ro");
        assert!(opts.volumes[1].read_only);
    }

    #[test]
    fn test_volume_flags_apply_to_windows_style() {
        let flags = VolumeFlags {
            volume: vec![
                r"C:\host\data:/guest/data".to_string(),
                r"D:\readonly:/ro:ro".to_string(),
            ],
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts, None).unwrap();
        assert_eq!(opts.volumes.len(), 2);
        assert_eq!(opts.volumes[0].host_path, r"C:\host\data");
        assert_eq!(opts.volumes[0].guest_path, "/guest/data");
        assert!(!opts.volumes[0].read_only);
        assert_eq!(opts.volumes[1].host_path, r"D:\readonly");
        assert_eq!(opts.volumes[1].guest_path, "/ro");
        assert!(opts.volumes[1].read_only);
    }

    /// `-v my-data:/data` reaches `BoxOptions` as a managed volume, not a host
    /// bind — the whole point of adopting Docker's rule. It must not touch the
    /// filesystem on the way: no canonicalize, no "path does not exist".
    #[test]
    fn test_volume_flags_apply_to_managed_volume() {
        let flags = VolumeFlags {
            volume: vec![
                "my-data:/data".to_string(),
                "vol_01K2EXAMPLE:/cache".to_string(),
            ],
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts, None).unwrap();

        assert_eq!(opts.volumes.len(), 2);
        assert_eq!(opts.volumes[0].managed_volume.as_deref(), Some("my-data"));
        assert_eq!(opts.volumes[0].host_path, "");
        assert_eq!(opts.volumes[0].guest_path, "/data");
        assert!(!opts.volumes[0].read_only);
        assert_eq!(
            opts.volumes[1].managed_volume.as_deref(),
            Some("vol_01K2EXAMPLE")
        );
        assert_eq!(opts.volumes[1].guest_path, "/cache");
    }

    /// `:ro` on a managed volume is refused, not quietly downgraded. Neither
    /// the server nor the REST client accepts one, and a caller who believes a
    /// mount is protected when it is writable is the failure worth preventing.
    #[test]
    fn test_volume_flags_reject_read_only_managed_volume() {
        let flags = VolumeFlags {
            volume: vec!["my-data:/data:ro".to_string()],
        };
        let mut opts = BoxOptions::default();

        let error = flags
            .apply_to(&mut opts, None)
            .expect_err("read-only managed volumes must be refused")
            .to_string();

        assert!(error.contains("read-only"), "{error}");
        assert!(error.contains("my-data"), "{error}");
        assert!(opts.volumes.is_empty());
    }

    /// A host bind may still be read-only — the restriction is specific to
    /// managed volumes, not to `:ro`.
    #[test]
    fn test_volume_flags_still_allow_read_only_host_binds() {
        let flags = VolumeFlags {
            volume: vec!["/host/data:/data:ro".to_string()],
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts, None).unwrap();

        assert_eq!(opts.volumes[0].host_path, "/host/data");
        assert!(opts.volumes[0].read_only);
    }

    /// A host bind keeps `managed_volume` unset, so the two `-v` forms stay
    /// distinguishable all the way to the wire.
    #[test]
    fn test_volume_flags_apply_to_leaves_host_binds_unmanaged() {
        let flags = VolumeFlags {
            volume: vec!["/host/data:/guest/data".to_string()],
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts, None).unwrap();

        assert_eq!(opts.volumes[0].managed_volume, None);
        assert_eq!(opts.volumes[0].host_path, "/host/data");
    }

    #[test]
    fn test_volume_flags_apply_to_anonymous() {
        let base = std::env::temp_dir();
        let flags = VolumeFlags {
            volume: vec!["/data".to_string(), "/cache:ro".to_string()],
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts, Some(&base)).unwrap();
        assert_eq!(opts.volumes.len(), 2);
        assert_eq!(opts.volumes[0].guest_path, "/data");
        assert!(
            opts.volumes[0].host_path.contains("anonymous"),
            "anonymous volume host_path should contain 'anonymous': {}",
            opts.volumes[0].host_path
        );
        assert!(std::path::Path::new(&opts.volumes[0].host_path).exists());
        assert_eq!(opts.volumes[1].guest_path, "/cache");
        assert!(opts.volumes[1].read_only);
        assert!(opts.volumes[1].host_path.contains("anonymous"));
    }

    // ─── auth subcommand parse tests ───────────────────────────────────────

    use crate::commands::{auth::AuthCommand, network::NetworkCommand};
    use clap::Parser;

    // ─── tunnel parse tests ────────────────────────────────────────────────

    #[test]
    fn tunnel_parses_box_and_port() {
        let cli =
            Cli::try_parse_from(["boxlite", "network", "tunnel", "mybox", "3000"]).expect("parse");
        let Commands::Network(network) = cli.command else {
            panic!("expected Commands::Network");
        };
        let NetworkCommand::Tunnel(args) = network.command;
        assert_eq!(args.target, "mybox");
        assert_eq!(args.port, 3000);
        assert!(args.listen.is_none());
    }

    #[test]
    fn network_tunnel_accepts_listener_forms() {
        for listen in [
            "8080",
            "0",
            "127.0.0.1:8080",
            "[::1]:8080",
            "unix:/tmp/app.sock",
        ] {
            Cli::try_parse_from([
                "boxlite", "network", "tunnel", "mybox", "3000", "--listen", listen,
            ])
            .unwrap_or_else(|error| panic!("{listen} should parse: {error}"));
        }
    }

    #[test]
    fn network_tunnel_rejects_ambiguous_listener_forms() {
        for listen in [
            "localhost:8080",
            "::1:8080",
            ":8080",
            "unix:relative.sock",
            "127.0.0.1",
        ] {
            let result = Cli::try_parse_from([
                "boxlite", "network", "tunnel", "mybox", "3000", "--listen", listen,
            ]);
            assert!(result.is_err(), "{listen} must be rejected");
        }
    }

    #[test]
    fn tunnel_is_not_a_top_level_command() {
        let result = Cli::try_parse_from(["boxlite", "tunnel", "mybox", "3000"]);
        assert!(result.is_err(), "tunnel must be nested under network");
    }

    #[test]
    fn tunnel_rejects_port_zero_at_parse() {
        let result = Cli::try_parse_from(["boxlite", "network", "tunnel", "mybox", "0"]);
        assert!(result.is_err(), "port 0 must be rejected by the parser");
    }

    /// `boxlite port BOX` existed earlier on this branch and was withdrawn:
    /// resolved bindings are reported through `boxlite inspect`, and remote
    /// access goes through `boxlite network tunnel`.
    #[test]
    fn withdrawn_port_subcommand_is_rejected() {
        let result = Cli::try_parse_from(["boxlite", "port", "mybox"]);
        assert!(
            result.is_err(),
            "the withdrawn port command must not come back"
        );
    }

    #[test]
    fn auth_login_parses_with_no_flags() {
        let cli = Cli::try_parse_from(["boxlite", "auth", "login"]).expect("parse");
        let Commands::Auth(args) = cli.command else {
            panic!("expected Commands::Auth");
        };
        assert!(matches!(args.command, AuthCommand::Login(_)));
    }

    #[test]
    fn auth_logout_parses() {
        let cli = Cli::try_parse_from(["boxlite", "auth", "logout"]).expect("parse");
        let Commands::Auth(args) = cli.command else {
            panic!("expected Commands::Auth");
        };
        assert!(matches!(args.command, AuthCommand::Logout(_)));
    }

    #[test]
    fn auth_status_parses() {
        let cli = Cli::try_parse_from(["boxlite", "auth", "status"]).expect("parse");
        let Commands::Auth(args) = cli.command else {
            panic!("expected Commands::Auth");
        };
        assert!(matches!(args.command, AuthCommand::Status));
    }

    // ─── volume subcommand parse tests ─────────────────────────────────────

    use crate::commands::volume::VolumeCommand;

    #[test]
    fn volume_create_takes_an_optional_name() {
        // Without --name the server names the volume after its id.
        let cli = Cli::try_parse_from(["boxlite", "volume", "create"]).expect("parse");
        let Commands::Volume(args) = cli.command else {
            panic!("expected Commands::Volume");
        };
        let VolumeCommand::Create(create) = args.command else {
            panic!("expected VolumeCommand::Create");
        };
        assert_eq!(create.name, None);

        let cli = Cli::try_parse_from(["boxlite", "volume", "create", "--name", "my-data"])
            .expect("parse");
        let Commands::Volume(args) = cli.command else {
            panic!("expected Commands::Volume");
        };
        let VolumeCommand::Create(create) = args.command else {
            panic!("expected VolumeCommand::Create");
        };
        assert_eq!(create.name.as_deref(), Some("my-data"));

        // The name is a flag, not a positional: a bare argument is still rejected.
        assert!(Cli::try_parse_from(["boxlite", "volume", "create", "data"]).is_err());
    }

    #[test]
    fn volume_ls_list_alias_parses() {
        // The `list` visible alias must resolve to the same Ls variant.
        for verb in ["ls", "list"] {
            let cli = Cli::try_parse_from(["boxlite", "volume", verb]).expect("parse");
            let Commands::Volume(args) = cli.command else {
                panic!("expected Commands::Volume for {verb}");
            };
            assert!(
                matches!(args.command, VolumeCommand::Ls(_)),
                "{verb} should map to Ls"
            );
        }
    }

    #[test]
    fn volume_get_inspect_alias_parses() {
        for verb in ["get", "inspect"] {
            let cli = Cli::try_parse_from(["boxlite", "volume", verb, "vol-123"]).expect("parse");
            let Commands::Volume(args) = cli.command else {
                panic!("expected Commands::Volume for {verb}");
            };
            let VolumeCommand::Get(get) = args.command else {
                panic!("{verb} should map to Get");
            };
            assert_eq!(get.id, "vol-123");
        }
    }

    #[test]
    fn volume_rm_takes_multiple_ids_and_force() {
        let cli = Cli::try_parse_from(["boxlite", "volume", "rm", "-f", "a", "b"]).expect("parse");
        let Commands::Volume(args) = cli.command else {
            panic!("expected Commands::Volume");
        };
        let VolumeCommand::Rm(rm) = args.command else {
            panic!("expected VolumeCommand::Rm");
        };
        assert!(rm.force);
        assert_eq!(rm.ids, vec!["a", "b"]);
    }

    #[test]
    fn volume_rm_requires_an_id() {
        // `num_args = 1..` + required means a bare `volume rm` must error.
        assert!(Cli::try_parse_from(["boxlite", "volume", "rm"]).is_err());
    }

    #[test]
    fn auth_login_api_key_stdin_parses() {
        // --api-key-stdin is the only non-interactive credential path
        // after the device-flow removal; it must parse cleanly.
        let cli = Cli::try_parse_from(["boxlite", "auth", "login", "--api-key-stdin"])
            .expect("--api-key-stdin should parse");
        let Commands::Auth(args) = cli.command else {
            panic!("expected Commands::Auth");
        };
        let AuthCommand::Login(login) = args.command else {
            panic!("expected AuthCommand::Login");
        };
        assert!(login.api_key_stdin);
    }

    // ============================================================
    // ManagementFlags --security
    //
    // Side A (setting valid) — `--security=disable` lands as
    // SecurityOptions::disabled() on the resulting BoxOptions.
    // Side B (setting invalid) — surfaces back as an
    // anyhow::Error pointing at the offending value. Reverting the
    // `from_preset(preset)?` call in apply_to flips both red.
    // ============================================================

    fn lifecycle_flags(
        auto_stop: Option<u32>,
        auto_delete: Option<u32>,
        no_auto_resume: bool,
    ) -> ManagementFlags {
        ManagementFlags {
            name: None,
            security: None,
            nested_virtualization: false,
            auto_stop,
            auto_delete,
            no_auto_resume,
        }
    }

    #[test]
    fn an_explicit_delete_deadline_survives_the_rm_sentinel() {
        // `apply_to` writes `auto_delete = Some(0)` unconditionally as the
        // "no --rm" default. Applying the flag before that line would have the
        // deadline silently overwritten by 0 — the box would never be swept.
        let mut opts = BoxOptions::default();
        lifecycle_flags(None, Some(3_600), false)
            .apply_to(&mut opts)
            .expect("flags must apply");

        assert_eq!(opts.auto_delete, Some(3_600));
    }

    #[test]
    fn lifecycle_flags_reach_box_options() {
        let mut opts = BoxOptions::default();
        lifecycle_flags(Some(900), None, true)
            .apply_to(&mut opts)
            .expect("flags must apply");

        assert_eq!(opts.auto_stop, Some(900));
        assert_eq!(opts.auto_resume, Some(false));
    }

    #[test]
    fn omitted_lifecycle_flags_leave_the_existing_defaults_alone() {
        // Absent flags must not invent policy: auto_stop and auto_resume stay
        // unset, and auto_delete keeps the historical `--rm` encoding.
        let mut opts = BoxOptions::default();
        lifecycle_flags(None, None, false)
            .apply_to(&mut opts)
            .expect("flags must apply");

        assert_eq!(opts.auto_stop, None);
        assert_eq!(opts.auto_resume, None);
        assert_eq!(opts.auto_delete, Some(0));
    }

    #[test]
    fn durations_accept_bare_seconds_and_suffixes() {
        assert_eq!(parse_duration_seconds("30"), Ok(30));
        assert_eq!(parse_duration_seconds("30s"), Ok(30));
        assert_eq!(parse_duration_seconds("15m"), Ok(900));
        assert_eq!(parse_duration_seconds("2h"), Ok(7_200));
        assert_eq!(parse_duration_seconds("7d"), Ok(604_800));
        assert_eq!(parse_duration_seconds("0"), Ok(0));

        for bad in ["", "m", "-5", "1.5h", "abc", "10x"] {
            assert!(
                parse_duration_seconds(bad).is_err(),
                "{bad:?} must not parse"
            );
        }
    }

    #[test]
    fn a_deadline_without_a_server_is_refused_not_silently_reinterpreted() {
        // Against the embedded runtime `--auto-stop` is rejected by the engine
        // and `--auto-delete` would quietly become remove-on-stop, deleting the
        // box at its stop instead of after the delay. Both must fail here, with
        // the flag named.
        for (auto_stop, auto_delete, flag) in [
            (Some(900), None, "--auto-stop"),
            (None, Some(3_600), "--auto-delete"),
        ] {
            let err = lifecycle_flags(auto_stop, auto_delete, false)
                .require_sweeper(false)
                .expect_err("a deadline with no sweeper must be refused");
            assert!(
                err.to_string().contains(flag),
                "error must name {flag}: {err}"
            );
        }
    }

    #[test]
    fn a_deadline_is_allowed_against_a_server_and_zero_is_always_allowed() {
        assert!(
            lifecycle_flags(Some(900), Some(3_600), false)
                .require_sweeper(true)
                .is_ok(),
            "a server runs the sweeper, so both deadlines are fine"
        );
        // `0` disables rather than requesting enforcement, so it needs nothing.
        assert!(
            lifecycle_flags(Some(0), Some(0), false)
                .require_sweeper(false)
                .is_ok(),
            "a disabled deadline asks nothing of the runtime"
        );
    }

    #[test]
    fn rm_and_auto_delete_cannot_be_combined() {
        // They mean different things — remove on stop versus remove after a
        // delay — and `apply_to` resolves them by letting --auto-delete win,
        // so the parser has to be what stops a caller asking for both.
        let conflict = Cli::try_parse_from([
            "boxlite",
            "run",
            "--rm",
            "--auto-delete",
            "1h",
            "alpine:latest",
        ]);

        assert!(
            conflict.is_err(),
            "--rm with --auto-delete must be rejected"
        );

        // `--rm` rides on the wire as `auto_delete = 1`, and the contract
        // requires `auto_delete > auto_stop`, so this pair could only ever fail
        // server-side complaining about a field the caller never set. The
        // parser has to be what says no.
        let with_auto_stop = Cli::try_parse_from([
            "boxlite",
            "run",
            "--rm",
            "--auto-stop",
            "60",
            "alpine:latest",
        ]);

        assert!(
            with_auto_stop.is_err(),
            "--rm with --auto-stop must be rejected"
        );
    }

    #[test]
    fn management_security_preset_applies_to_box_options() {
        let flags = ManagementFlags {
            name: None,
            security: Some("disable".to_string()),
            nested_virtualization: false,
            auto_stop: None,
            auto_delete: None,
            no_auto_resume: false,
        };
        let mut opts = BoxOptions::default();
        flags.apply_to(&mut opts).expect("setting must apply");
        assert_eq!(
            opts.advanced.security,
            boxlite::SecurityOptions::disabled(),
            "advanced.security must equal SecurityOptions::disabled()"
        );
    }

    #[test]
    fn management_security_preset_absent_leaves_default() {
        let flags = ManagementFlags {
            name: None,
            security: None,
            nested_virtualization: false,
            auto_stop: None,
            auto_delete: None,
            no_auto_resume: false,
        };
        let mut opts = BoxOptions::default();
        flags
            .apply_to(&mut opts)
            .expect("absent preset must succeed");
        assert_eq!(
            opts.advanced.security,
            boxlite::SecurityOptions::default(),
            "absent preset must leave the default in place"
        );
    }

    #[test]
    fn management_security_preset_typo_surfaces_anyhow_error() {
        let flags = ManagementFlags {
            name: None,
            security: Some("ultra".to_string()),
            nested_virtualization: false,
            auto_stop: None,
            auto_delete: None,
            no_auto_resume: false,
        };
        let mut opts = BoxOptions::default();
        let err = flags
            .apply_to(&mut opts)
            .expect_err("unknown preset must reject at apply_to");
        let msg = err.to_string();
        assert!(msg.contains("ultra"), "got {msg}");
    }

    #[test]
    fn nested_virtualization_flag_applies_to_box_options() {
        let cli =
            Cli::try_parse_from(["boxlite", "run", "--nested-virtualization", "alpine:latest"])
                .expect("nested virtualization flag should parse");
        let Commands::Run(args) = cli.command else {
            panic!("expected run command");
        };

        let mut opts = BoxOptions::default();
        assert!(!opts.advanced.nested_virtualization);
        args.management
            .apply_to(&mut opts)
            .expect("nested virtualization should apply");

        assert!(opts.advanced.nested_virtualization);
    }
}
