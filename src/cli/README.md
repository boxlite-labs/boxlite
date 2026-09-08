# BoxLite CLI

Command-line interface for BoxLite — use BoxLite without writing code, with a familiar Docker/Podman-like experience.


For CLI development (build, test, adding commands), see [CLI Development Guide](../docs/development/cli.md).


**Platforms:** macOS (Apple Silicon), Linux (x86_64, ARM64)

## Overview

The BoxLite CLI (`boxlite`) lets you create, run, and manage BoxLite boxes from the terminal. It targets quick testing, shell scripting and automation, debugging, and demos.


### Key Features

- **Run** — Create a box from an image or prepared rootfs and run a command (interactive, TTY, or detached); supports `-p` (publish ports) and `-v` (volumes)
- **Create** — Create a box from an image or prepared rootfs without running; supports `-p` and `-v`
- **Lifecycle** — Start, stop, restart, remove boxes
- **Inspect** — Show detailed box info (JSON, YAML, or Go template)
- **Exec** — Run commands in a running or safely resumable box
- **Images** — Pull and list OCI images
- **Copy** — Copy files between host and box (`boxlite cp`)
- **Output formats** — Table, JSON, or YAML for list/images
- **Shell completion** — Bash, Zsh, Fish

## Installation

### One-line install (Linux & macOS Apple Silicon)

```bash
curl -fsSL https://sh.boxlite.ai | sh
```

Installs to `$HOME/.local/bin/boxlite`. For version pinning, a custom
install dir, and release-artifact verification (sigstore, `SHA256SUMS`,
`gh attestation verify`), see the [CLI Reference's Installation & Verification section](../../docs/reference/cli/README.md#installation--verification).

### cargo install (from source)

```bash
cargo install boxlite-cli
```

### cargo binstall (prebuilt binary)

```bash
cargo binstall boxlite-cli
```

### Homebrew

Coming soon

### Build from Source

```bash
# From repository root
git clone https://github.com/boxlite-ai/boxlite.git
cd boxlite

# Initialize submodules (required)
git submodule update --init --recursive

# Build the CLI
cargo build --release -p boxlite-cli

# Binary: target/release/boxlite
```

### System Requirements

| Platform       | Architecture          | Status           |
|----------------|-----------------------|------------------|
| macOS          | Apple Silicon (ARM64) | ✅ Supported     |
| Linux          | x86_64                | ✅ Supported     |
| Linux          | ARM64                 | ✅ Supported     |
| Windows (WSL2) | x86_64                | ✅ Supported     |
| macOS          | Intel (x86_64)        | ❌ Not supported |


## Quick Start

### Run a one-off command

```bash
boxlite run python:slim python -c "print('Hello from BoxLite!')"
```

### Run interactively with a TTY

```bash
boxlite run -it alpine:latest /bin/sh
```

### Create a box and run in the background

```bash
# Create and start (prints box ID)
boxlite run -d --name mybox alpine:latest sleep 3600

# Run a command in the box
boxlite exec mybox echo "Hello"

# List boxes
boxlite list -a

# Stop and remove
boxlite stop mybox
boxlite rm mybox
```

### Pull an image and list images

```bash
boxlite pull alpine:latest
boxlite images
```

## Connecting to a remote server

To target a remote BoxLite REST server instead of the local runtime, sign in
with `boxlite auth login`. Three login methods are supported:

| Method      | When to use                                            | Token type           |
|-------------|--------------------------------------------------------|----------------------|
| `api-key`   | CI / automation, or a server-minted long-lived key     | Opaque `blk_…` key   |
| `browser`   | Local developer machine with a desktop browser         | OIDC access token    |
| `device`    | SSH session / headless container with no browser       | OIDC access token    |

Credential precedence is **`BOXLITE_API_KEY` env > stored file > unauthenticated** (local runtime).
The `--url` flag overrides the URL specifically without affecting credentials.
Multiple profiles coexist in one file via `--profile <name>` (or `BOXLITE_PROFILE` env var).

```bash
# Browser OIDC against a control plane (default for `auth login` on a TTY).
# Requires the IdP admin to have registered `http://127.0.0.1:5555/callback`
# in the SPA application's Allowed Callback URLs — see apps/infra/README.md
# "Callback URL mismatch" for the one-time setup.
boxlite --profile cloud auth login --url https://<your-control-plane>/api

# Same target, headless (SSH / no browser): prints a code + URL to type into
# any browser on another device.
boxlite --profile cloud auth login --url https://<your-control-plane>/api --no-browser

# Local boxlite serve with a static API key (unchanged from the original behavior).
boxlite --profile local auth login --url http://localhost:8100  # interactive paste
echo "$KEY" | boxlite --profile local auth login --url http://localhost:8100 --api-key-stdin

# CI via env vars only — no `auth login` call needed.
BOXLITE_API_KEY=$KEY BOXLITE_REST_URL=https://<your-server> boxlite list
```

Credentials are stored at `<BOXLITE_HOME>/credentials.toml` (default
`~/.boxlite/credentials.toml`, perms `0600`). OIDC
sessions auto-refresh on use when within 5 minutes of expiry; if the refresh
token is rejected (`invalid_grant`) the CLI prompts you to re-run `auth login`.

## Commands Reference

> For an exhaustive man-page-style reference (shared flag groups, volume/port grammar,
> exit codes, configuration file format), see the
> [CLI Reference](../../docs/reference/cli/README.md).

### Global flags

Place these options before the top-level command or after the complete command
path. A parent command accepts only options shared by all of its children, so
write `boxlite auth status --url URL` (or `boxlite --url URL auth status`), not
`boxlite auth --url URL status`. Each command's help shows only options it can
use. Executing invocations reject unsupported options; Clap's `--help` and
`--version` display actions remain terminal.

| Flag | Scope | Description |
|------|-------|-------------|
| `--debug` | all commands except `completion` | Enable debug output. Precedence: `--debug` > `RUST_LOG` env > default (`warn`). |
| `--home PATH` | local runtime + credentials | Absolute BoxLite home directory (default: `~/.boxlite`). `BOXLITE_HOME` is the env spelling. |
| `--registry REGISTRY` | `run`, `create`, `pull`, `serve` | Image registry (repeatable; prepended to config). |
| `--config PATH` | local-capable commands | JSON config file path (e.g. for `home_dir` and `image_registries`). |
| `--url URL` | REST-capable commands | Connect to a remote BoxLite REST server instead of the local runtime. Env: `BOXLITE_REST_URL`. |
| `--profile NAME` | REST-capable commands + `auth` | Named credential profile in `<BOXLITE_HOME>/credentials.toml`. Default `default`. Env: `BOXLITE_PROFILE`. |
| `--path-prefix VALUE` | REST box/volume commands | Routing-slot value for `/v1/<prefix>/...`; overrides the selected profile. Env: `BOXLITE_REST_PATH_PREFIX`. |

### `boxlite auth login`

Log in to a BoxLite REST server. Supports three flows that all save to
`<BOXLITE_HOME>/credentials.toml` (default `~/.boxlite/credentials.toml`,
perms `0600`):

- **API key** — paste or stdin. Long-lived, org-scoped. Good for CI, SDK
  integrations, `boxlite serve` setups.
- **Browser OIDC** — Authorization Code + PKCE against the IdP that
  `apps/api` is configured for (Auth0, Dex, Okta, etc.). Opens the system
  browser; the CLI listens on `127.0.0.1:5555` for the callback. Mints an
  access token + refresh token; the latter is used to silently refresh
  within 5 minutes of expiry.
- **Device code OIDC** — RFC 8628. Headless / SSH-friendly: prints a short
  code + URL to enter on any browser on another device. Same token type
  and refresh behavior as the browser flow.

On a BoxLite Auth0 deployment, an existing unverified database account must
complete the hosted verification Form through browser login:

```bash
boxlite --profile cloud auth login --url https://<your-control-plane>/api --method browser
```

Auth0 Forms cannot run inside refresh-token, device-code, or other
non-interactive exchanges. An SSH user should finish verification through the
BoxLite dashboard in a separate browser, then retry the device login.

When `--method` is unset, the CLI infers it: piped stdin → `api-key` (CI-safe),
while a TTY opens the interactive picker. If Browser is selected from that
picker in a detected headless/SSH environment, the CLI falls back to `device`.

**Usage:** `boxlite auth login [OPTIONS]`

| Option | Description |
|--------|-------------|
| `--url URL` | Server URL (precedence: flag, `BOXLITE_REST_URL`, selected profile URL, then `http://localhost:8100`). For cloud control planes include the `/api` prefix. |
| `--method <api-key\|browser\|device>` | Explicit flow choice. Overrides inference. |
| `--api-key-stdin` | Read the API key from stdin (one line). The flag takes no value, so the secret never appears on argv. Selects API-key login and conflicts with explicit browser/device methods. |
| `--no-browser` | Use device code when no method is explicit; conflicts with API-key stdin and explicit API-key/browser methods. |
| `--callback-port <PORT>` | Local port for the browser-flow callback (default `5555`). **Must match an entry in the IdP's allow-list byte-for-byte** — a different port produces "Callback URL mismatch" exactly like no entry at all. |
| `--issuer URL` | OIDC issuer URL. Overrides what `GET /api/config` returns. Useful for self-hosted Dex tenants where the discovery is wrong. |
| `--client-id ID` | OIDC client_id. Overrides `/api/config`. |
| `--audience VAL` | OIDC audience. Auth0 requires it; Dex tolerates `None`. |

`--profile NAME` selects the credential profile (default `default`), and
`--home PATH` selects the credential-store root.

**Examples:**

```bash
# Cloud control plane via browser (most common; opens system browser).
boxlite --profile cloud auth login --url https://<your-control-plane>/api

# Same target, headless: prints a code + URL.
boxlite --profile cloud auth login --url https://<your-control-plane>/api --no-browser

# Local boxlite serve with paste-API-key (interactive).
boxlite --profile local auth login --url http://localhost:8100

# CI: API key from stdin (nothing on argv).
echo "$KEY" | boxlite --profile local auth login --url http://localhost:8100 --api-key-stdin
```

**Deployment-side setup (one-time, by the IdP admin):**

Browser and device flows fail with "Callback URL mismatch" until the IdP
knows about the CLI's loopback URL. For Auth0 see
`apps/infra/README.md` "Callback URL mismatch" — add
`http://127.0.0.1:5555/callback` to the SPA Application's
**Allowed Callback URLs**. For Dex see `apps/dex/config.yaml` — the same
URL goes under the `boxlite` static client's `redirectURIs`, plus
`oauth2.deviceFlow: {}` at the top level for device flow.

### `boxlite auth logout`

Remove the selected profile from `<BOXLITE_HOME>/credentials.toml` (default
`~/.boxlite/credentials.toml`). Prompts for confirmation unless `--yes` is given.

**Usage:** `boxlite auth logout [OPTIONS]`

| Option | Short | Description |
|--------|-------|-------------|
| `--yes` | `-y` | Skip the confirmation prompt |

### `boxlite auth status`

Print the current authentication state: the logged-in URL, the source
(stored file vs env var), the credential type (API key vs OIDC), and for
OIDC sessions the access token's expiry. Offline — no network calls,
no secret material printed.

**Usage:** `boxlite auth status [--url URL] [--profile NAME]`

**Example output (API key):**

```
Logged in to:    http://localhost:8100
Credential:      API key (from <BOXLITE_HOME>/credentials.toml [local])
```

**Example output (OIDC session):**

```
Logged in to:    https://api.boxlite.ai/api
Credential:      OIDC bearer token (from <BOXLITE_HOME>/credentials.toml [cloud])
Expires:         2026-05-21T15:42:00+00:00
```

### `boxlite auth whoami`

Confirm the active credential's identity by making one authenticated
request to `GET /v1/me`. Unlike `auth status` (offline, only reports where
the credential came from), `whoami` shows the server-resolved principal,
path prefix, and scopes. Triggers a silent OIDC refresh if the access
token is within 5 minutes of expiry.

**Usage:** `boxlite auth whoami [--url URL] [--profile NAME]`

**Example output:**

```
Logged in as:    dev@acme.test
Name:            Dev McAcme
Principal:       auth0|abc123 (user)
Path prefix:     acme
Server:          https://api.boxlite.ai/api
Scopes:          box:read, box:write, box:exec, image:read, snapshot:read
```

### `boxlite run`

Create a box from an image (or a prepared rootfs via `--rootfs`) and run a
command, with docker's semantics: `COMMAND` replaces the image's `CMD`, the
image's `ENTRYPOINT` is prepended, and the result **is** the container's init
(PID 1). Omit it and the image's own default runs.

The box lives exactly as long as that command. When it exits, the box stops and
takes its exit code — `boxlite inspect -f '{{.State.ExitCode}}' NAME` reads it
back. `exec` against a box whose command has finished is refused rather than
silently restarting it, because restarting would run the command a second time.

**Usage:**

- `boxlite run [OPTIONS] IMAGE [COMMAND]...`
- `boxlite run [OPTIONS] --rootfs PATH [COMMAND]...`

| Option | Short | Description |
|--------|-------|-------------|
| `--rootfs PATH` | | Use a prepared rootfs path instead of pulling/resolving an image |
| `--interactive` | `-i` | Keep STDIN open; conflicts with `--detach` |
| `--tty` | `-t` | Allocate a pseudo-TTY; requires TTY-attached stdin, even with `--detach` |
| `--env KEY[=VALUE]` | `-e` | Set environment variables; a bare key inherits its host value (repeatable) |
| `--workdir PATH` | `-w` | Working directory in the box |
| `--user NAME[:GROUP]` | `-u` | Run as a name/uid and optional group/gid |
| `--entrypoint EXEC` | | Override the image entrypoint |
| `--publish PORT` | `-p` | Publish a TCP box port locally (`80` = automatic host port, `8080:80` = fixed) |
| `--volume VOLUME` | `-v` | Mount a volume: `name:/box` for a managed volume, `./path:/box` for a host bind, `/box` for anonymous |
| `--cpus N` | | CPU limit |
| `--memory MiB` | | Memory limit (MiB) |
| `--disk-size GB` | | Sparse rootfs disk size; smaller values than the base image are ignored |
| `--cap-add CAPABILITY` | | Add a Linux capability (repeatable; accepts `CAP_` prefix or `ALL`) |
| `--cap-drop CAPABILITY` | | Drop a Linux capability (repeatable; accepts `CAP_` prefix or `ALL`) |
| `--network <enabled\|disabled>` | | Outbound network mode (default `enabled`) |
| `--allow-net HOST` | | Restrict egress to an exact host, wildcard domain, IP, or CIDR; repeatable and implies enabled networking |
| `--inbound <enabled\|disabled>` | | Inbound network mode (default `enabled`) |
| `--net-tx-kbps KBPS` | | Cap what the box sends, in kilobits/sec; `0` or unset is uncapped. Local runtime only; verified on Linux |
| `--net-rx-kbps KBPS` | | Cap what reaches the box, in kilobits/sec; `0` or unset is uncapped. Local runtime only; verified on Linux |
| `--name NAME` | | Name the box |
| `--detach` | `-d` | Run in background, print box ID |
| `--rm` | | Remove the box when it stops; conflicts with lifecycle deadlines and is ignored with `--detach` |
| `--auto-stop DURATION` | | Stop the box after this much inactivity; `0` disables |
| `--auto-delete DURATION` | | Delete the box this long after it stops; `0` disables |
| `--no-auto-resume` | | Ask a REST server to refuse implicit resume after a box has run; first boot is unaffected, and the embedded runtime records but does not enforce it |
| `--security <enable\|disable>` | | Embedded-runtime sandbox security (default `enable`; env `BOXLITE_SECURITY`); REST servers own this policy |

`-p` is explicit local publication. Remote REST profiles reject it and direct
the caller to `boxlite network tunnel`.

`DURATION` is seconds when bare, or suffixed: `30s`, `15m`, `2h`, `7d`. The two
deadlines are swept by a server — `boxlite serve` or the cloud — so against the
embedded runtime they are refused rather than silently reinterpreted. See the
[CLI reference](../../docs/reference/cli/README.md) for the sweep's granularity
and how `--rm` differs between the two.

**Examples:**

```bash
boxlite run alpine:latest echo "Hello"
boxlite run -it --rm alpine:latest /bin/sh
boxlite run -d --name openclaw -p 18789:18789 ghcr.io/openclaw/openclaw:main
boxlite run -v /host/data:/app/data alpine:latest cat /app/data/hello.txt
boxlite run --rootfs /path/to/rootfs /bin/sh
boxlite run --cap-drop ALL --cap-add NET_BIND_SERVICE nginx:alpine
```

### `boxlite create`

Create a box without starting it.

`COMMAND` is stored, not run — it becomes the box's main command when the box is
next started, exactly as under `run`. A box created *with* one is a job, and
`exec`/`cp` will not start it implicitly (that would run the command); start it
deliberately with `boxlite start`. A box created *without* one boots the image's
default, and `exec` still starts it on demand.

**Usage:**

- `boxlite create [OPTIONS] IMAGE [COMMAND]...`
- `boxlite create [OPTIONS] --rootfs PATH [COMMAND]...`

| Option | Short | Description |
|--------|-------|-------------|
| `--rootfs PATH` | | Use a prepared rootfs path instead of pulling/resolving an image |
| `--name NAME` | | Name the box |
| `--env KEY[=VALUE]` | `-e` | Environment variables; a bare key inherits its host value |
| `--workdir PATH` | `-w` | Working directory |
| `--entrypoint EXEC` | | Override the image entrypoint |
| `--publish PORT` | `-p` | Publish a TCP box port locally (`80` = automatic host port, `8080:80` = fixed) |
| `--volume VOLUME` | `-v` | Mount a volume: `name:/box` for a managed volume, `./path:/box` for a host bind, `/box` for anonymous |
| `--cpus N` | | CPU limit |
| `--memory MiB` | | Memory limit (MiB) |
| `--disk-size GB` | | Sparse rootfs disk size; smaller values than the base image are ignored |
| `--cap-add CAPABILITY` | | Add a Linux capability (repeatable; accepts `CAP_` prefix or `ALL`) |
| `--cap-drop CAPABILITY` | | Drop a Linux capability (repeatable; accepts `CAP_` prefix or `ALL`) |
| `--network <enabled\|disabled>` | | Outbound network mode (default `enabled`) |
| `--allow-net HOST` | | Restrict egress to an exact host, wildcard domain, IP, or CIDR; repeatable and implies enabled networking |
| `--inbound <enabled\|disabled>` | | Inbound network mode (default `enabled`) |
| `--net-tx-kbps KBPS` | | Cap what the box sends, in kilobits/sec; `0` or unset is uncapped. Local runtime only; verified on Linux |
| `--net-rx-kbps KBPS` | | Cap what reaches the box, in kilobits/sec; `0` or unset is uncapped. Local runtime only; verified on Linux |
| `--auto-stop DURATION` | | Stop the box after this much inactivity; `0` disables; requires a REST server |
| `--auto-delete DURATION` | | Delete the box this long after it stops; `0` disables; requires a REST server |
| `--no-auto-resume` | | Ask a REST server to refuse implicit resume after a box has run; first boot is unaffected, and the embedded runtime records but does not enforce it |
| `--security <enable\|disable>` | | Embedded-runtime sandbox security (default `enable`; env `BOXLITE_SECURITY`); REST servers own this policy |

`-p` is explicit local publication. Remote REST profiles reject it and direct
the caller to `boxlite network tunnel`.

**Examples:**

```bash
boxlite create --name mybox alpine:latest
boxlite create -p 18789:18789 -v /data:/app/data --name openclaw ghcr.io/openclaw/openclaw:main
boxlite create --rootfs /path/to/rootfs --name local-rootfs
boxlite create --cap-drop NET_RAW --name hardened alpine:latest
boxlite start mybox
boxlite start openclaw
```

### `boxlite exec`

Run a command in a box. A safely resumable stopped box starts implicitly; a
job box whose start would run or re-run its user-selected main command is
refused, so start that box deliberately with `boxlite start` first.

**Usage:** `boxlite exec [OPTIONS] BOX -- COMMAND [ARGS]...`

| Option | Short | Description |
|--------|-------|-------------|
| `--interactive` | `-i` | Keep STDIN open; conflicts with `--detach` |
| `--tty` | `-t` | Allocate a TTY; foreground mode requires TTY-attached stdin, while detached mode attaches no stdin |
| `--env KEY[=VALUE]` | `-e` | Environment variables; a bare key inherits its host value |
| `--workdir PATH` | `-w` | Working directory |
| `--user NAME[:GROUP]` | `-u` | Run as a name/uid and optional group/gid |
| `--detach` | `-d` | Run in background (don’t wait) |

**Example:**

```bash
boxlite exec -it mybox -- /bin/sh
```

### `boxlite list` (alias: `ls`, `ps`)

List boxes.

**Usage:** `boxlite list [OPTIONS]`

| Option | Short | Description |
|--------|-------|-------------|
| `--all` | `-a` | Show all boxes (default: running only) |
| `--quiet` | `-q` | Show only IDs; conflicts with `--format` |
| `--format FMT` | | Output format: `table`, `json`, `yaml`; conflicts with `--quiet` (default: `table`) |

### `boxlite start`

Start one or more stopped boxes.

**Usage:** `boxlite start BOX [BOX ...]`

### `boxlite stop`

Stop one or more running boxes.

**Usage:** `boxlite stop BOX [BOX ...]`

### `boxlite restart`

Restart one or more boxes.

**Usage:** `boxlite restart BOX [BOX ...]`

### `boxlite rm`

Remove one or more boxes.

**Usage:** `boxlite rm [OPTIONS] BOX [BOX ...]` or `boxlite rm [OPTIONS] --all`

| Option | Short | Description |
|--------|-------|-------------|
| `--force` | `-f` | Force remove (e.g. running box) |
| `--all` | `-a` | Remove all boxes (prompts unless `--force`) |

### `boxlite pull`

Pull an image from a registry.

**Usage:** `boxlite pull [OPTIONS] IMAGE`

| Option | Short | Description |
|--------|-------|-------------|
| `--quiet` | `-q` | Only print digest |

### `boxlite inspect`

Display detailed information on one or more boxes (JSON, YAML, or Go-style template).

**Usage:** `boxlite inspect [OPTIONS] [BOX ...]` or `boxlite inspect --latest`

| Option | Short | Description |
|--------|-------|-------------|
| `--latest` | `-l` | Inspect the most recently created box (cannot be used with BOX) |
| `--format FMT` | `-f` | Output: `json`, `yaml`, or a Go template (e.g. `{{.State.Status}}`, `{{.Id}}`). Default: `json`. Table format is not supported. |

**Examples:**

```bash
boxlite inspect mybox
boxlite inspect -f '{{.State.Status}}' mybox
boxlite inspect --latest -f yaml
boxlite inspect box1 box2 -f json
```

### `boxlite images`

List cached images.

**Usage:** `boxlite images [OPTIONS]`

| Option | Short | Description |
|--------|-------|-------------|
| `--quiet` | `-q` | Show only image IDs; conflicts with `--format` |
| `--format FMT` | | Output format: `table`, `json`, `yaml`; conflicts with `--quiet` |

### `boxlite cp`

Copy files or directories between host and box.

**Usage:** `boxlite cp [OPTIONS] SRC DST`

- **SRC / DST:** host path or `BOX:PATH` (e.g. `mybox:/app/data`).

| Option | Description |
|--------|-------------|
| `--follow-symlinks` | Follow symlinks when copying (local runtime only) |
| `--no-overwrite` | Do not overwrite existing files (local runtime only) |
| `--no-include-parent` | Copy directory contents without their parent (local runtime only) |

**Examples:**

```bash
boxlite cp ./local.txt mybox:/workspace/
boxlite cp mybox:/app/out ./output
```

Paths at or under a mount inside the box are refused in both directions — `/tmp`,
`/dev/shm`, volumes, and the `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf` binds.
`cp` works on the rootfs layer from outside the box's mount namespace, so such a
copy would write where nothing in the box can see it, or read back the image's file
rather than the one the box has.

A directory that merely *contains* a mount differs by direction: copying one **out** is
refused (`boxlite cp box:/etc ./etc`), while copying **in** is allowed and refused only
if a file would land on a mount — so `boxlite cp ./x box:/etc` works.

Copy a path outside the mount, or pipe a tar through
`boxlite exec`. Files copied in are owned by the box's exec user.

### `boxlite info`

Display system-wide runtime information (version, paths, host/virtualization, box and image counts). Default output is YAML.

**Usage:** `boxlite info [OPTIONS]`

| Option | Description |
|--------|-------------|
| `--format FMT` | Output format: `yaml`, `json` (default: `yaml`). Table format is not supported. |

**Output fields:** `version`, `homeDir`, `virtualization`, `os`, `arch`, `boxesTotal`, `boxesRunning`, `boxesStopped`, `boxesConfigured`, `imagesCount`.

**Examples:**

```bash
boxlite info
boxlite info --format json
```

## Shell completion

Generate completion scripts for your shell:

```bash
# Bash
boxlite completion bash > /etc/bash_completion.d/boxlite
# or for current user
boxlite completion bash > ~/.local/share/bash-completion/completions/boxlite

# Zsh
boxlite completion zsh > "${fpath[1]}/_boxlite"

# Fish
boxlite completion fish > ~/.config/fish/completions/boxlite.fish
```

Then reload your shell or source the file.

## Environment variables

| Variable | Description |
|----------|-------------|
| `BOXLITE_HOME` | Runtime and credential home directory (default: `~/.boxlite`). Overridden by `--home`. |
| `BOXLITE_REST_URL` | REST server endpoint; equivalent to `--url`. |
| `BOXLITE_API_KEY` | Long-lived API key sent as `Authorization: Bearer`. Overrides any stored credentials. |
| `BOXLITE_PROFILE` | Credential profile; equivalent to `--profile` (default `default`). |
| `BOXLITE_REST_PATH_PREFIX` | REST routing path segment; equivalent to `--path-prefix`. |
| `BOXLITE_SECURITY` | `run`/`create` sandbox preset; equivalent to `--security`. |
| `BOXLITE_SERVE_API_KEY` | Expected bearer key for `serve`; equivalent to `serve --api-key`. |
| `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_AUDIENCE` | OIDC login fallbacks after flags and server discovery. |
| `BOXLITE_LOG_FILE` | Explicit JSON log-file path for any command; `serve` otherwise defaults to `<BOXLITE_HOME>/logs/serve.log`. |
| `BOXLITE_EXPERIMENTAL` | Comma-separated RC features: `custom-kernel`, `nested-virtualization`. |
| `BOXLITE_RECONNECT_GRACE` | `serve` reconnect grace (default `5m`). |
| `BOXLITE_SHUTDOWN_GRACE` | `serve` shutdown-stage grace (default `30s`). |
| `BOXLITE_MAX_SESSION_LIFETIME` | `serve` session hard cap (default `24h`). |
| `SSH_CONNECTION` | `auth login` headless detection; choosing Browser from the TTY picker falls back to device flow when set. |
| `RUST_LOG` | Log level: `trace`, `debug`, `info`, `warn`, `error`. Use `RUST_LOG=debug` for troubleshooting. |

## Configuration file

Use `--config PATH` to load a JSON config file. Useful for default registries and other options. See [Image registry configuration](../../docs/guides/image-registry-configuration.md) for details.

## Troubleshooting

### Image pull fails
- Check network and registry access.
- For private registries, see [Image registry configuration](../../docs/guides/image-registry-configuration.md) for details.
- **"Failed to pull manifest"** or **"error sending request for url"** (e.g. to `index.docker.io`): often network-related or Docker Hub rate limit/access in some regions. Retry later, use a mirror, or configure registries via `--registry` / `--config`. See [issue #190](https://github.com/boxlite-ai/boxlite/issues/190) for discussion.
- Enable debug output: `boxlite --debug pull IMAGE` or `RUST_LOG=debug boxlite pull IMAGE`.

### Box fails to start
- Enable debug output: `boxlite --debug run IMAGE [COMMAND]...` or `RUST_LOG=debug boxlite run IMAGE [COMMAND]...`.



## Further documentation

- [BoxLite README](../../README.md) — Project overview and SDK quick starts
- [Getting started](../../docs/getting-started/README.md) — Prerequisites and platform setup
- [Reference](../../docs/reference/README.md) — Python, Node, Rust, C API reference


## License

Licensed under the Apache License, Version 2.0. See [LICENSE](../LICENSE) for details.
