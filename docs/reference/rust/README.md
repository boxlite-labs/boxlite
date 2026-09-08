# Rust API Reference

Complete API reference for the BoxLite Rust SDK.

## Overview

The Rust SDK is the core implementation of BoxLite. It provides async-first APIs built on Tokio for creating and managing isolated VM environments.

**Crate**: `boxlite`
**Repository**: [github.com/anthropics/boxlite](https://github.com/anthropics/boxlite)

---

## Table of Contents

- [Runtime Management](#runtime-management)
  - [BoxliteRuntime](#boxliteruntime)
  - [BoxliteOptions](#boxliteoptions)
- [Box Handle](#box-handle)
  - [LiteBox](#litebox)
  - [BoxInfo](#boxinfo)
  - [BoxStatus](#boxstatus)
  - [BoxState](#boxstate)
- [Network Tunnels](#network-tunnels)
- [Command Execution](#command-execution)
  - [BoxCommand](#boxcommand)
  - [Execution](#execution)
  - [ExecStdin](#execstdin)
  - [ExecStdout / ExecStderr](#execstdout--execstderr)
  - [ExecResult](#execresult)
- [Box Configuration](#box-configuration)
  - [BoxOptions](#boxoptions)
  - [AdvancedBoxOptions](#advancedoptions)
  - [RootfsSpec](#rootfsspec)
  - [VolumeSpec](#volumespec)
  - [NetworkSpec](#networkspec)
  - [NetBandwidth](#netbandwidth)
  - [PortSpec](#portspec)
- [Security](#security)
  - [SecurityOptions](#securityoptions)
  - [SecurityOptionsBuilder](#securityoptionsbuilder)
  - [ResourceLimits](#resourcelimits)
- [Metrics](#metrics)
  - [RuntimeMetrics](#runtimemetrics)
  - [BoxMetrics](#boxmetrics)
- [Type Utilities](#type-utilities)
  - [Bytes](#bytes)
  - [Seconds](#seconds)
  - [BoxID](#boxid)
  - [ContainerID](#containerid)
- [Error Types](#error-types)
  - [BoxliteError](#boxliteerror)
  - [BoxliteResult](#boxliteresult)

---

## Runtime Management

### BoxliteRuntime

Main entry point for creating and managing boxes.

```rust
use boxlite::runtime::{BoxliteRuntime, BoxliteOptions, BoxOptions};
use boxlite::ImageRegistry;

// Create with default options
let runtime = BoxliteRuntime::with_defaults()?;

// Create with custom options
let options = BoxliteOptions {
    home_dir: PathBuf::from("/custom/boxlite"),
    image_registries: vec![
        ImageRegistry::https("ghcr.io/myorg").with_search(true),
        ImageRegistry::https("registry.example.com").with_basic_auth("user", "password"),
    ],
};
let runtime = BoxliteRuntime::new(options)?;

// Use global default runtime
let runtime = BoxliteRuntime::default_runtime();
```

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `fn new(options: BoxliteOptions) -> BoxliteResult<Self>` | Create runtime with options |
| `with_defaults` | `fn with_defaults() -> BoxliteResult<Self>` | Create with default options |
| `default_runtime` | `fn default_runtime() -> &'static Self` | Get/create global singleton |
| `try_default_runtime` | `fn try_default_runtime() -> Option<&'static Self>` | Get global if initialized |
| `init_default_runtime` | `fn init_default_runtime(options: BoxliteOptions) -> BoxliteResult<()>` | Initialize global with options |
| `create` | `async fn create(&self, options: BoxOptions, name: Option<String>) -> BoxliteResult<LiteBox>` | Create a new box |
| `get` | `async fn get(&self, id_or_name: &str) -> BoxliteResult<Option<LiteBox>>` | Get box by ID or name |
| `get_info` | `async fn get_info(&self, id_or_name: &str) -> BoxliteResult<Option<BoxInfo>>` | Get box info without handle |
| `list_info` | `async fn list_info(&self) -> BoxliteResult<Vec<BoxInfo>>` | List all boxes |
| `exists` | `async fn exists(&self, id_or_name: &str) -> BoxliteResult<bool>` | Check if box exists |
| `metrics` | `async fn metrics(&self) -> RuntimeMetrics` | Get runtime-wide metrics |
| `remove` | `async fn remove(&self, id_or_name: &str, force: bool) -> BoxliteResult<()>` | Remove box completely |

#### Example

```rust
use boxlite::runtime::{BoxliteRuntime, BoxOptions};
use boxlite::BoxCommand;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = BoxliteRuntime::with_defaults()?;

    // Create a box
    let options = BoxOptions::default();
    let litebox = runtime.create(options, Some("my-box".to_string())).await?;

    // Run a command
    let mut run = litebox.run(BoxCommand::new("echo").arg("Hello")).await?;
    let result = run.wait().await?;

    println!("Exit code: {}", result.exit_code);

    // Stop the box
    litebox.stop().await?;

    Ok(())
}
```

### BoxliteOptions

Runtime configuration options.

```rust
pub struct BoxliteOptions {
    /// Home directory for runtime data (~/.boxlite by default)
    pub home_dir: PathBuf,

    /// Registry transport, TLS, search, and auth configuration
    pub image_registries: Vec<ImageRegistry>,
}

pub struct ImageRegistry {
    /// Registry host name, optionally including a port. Do not include a URL scheme.
    pub host: String,
    /// `Https` by default; use `Http` for plain HTTP registries.
    pub transport: RegistryTransport,
    /// Disable TLS certificate and hostname verification for HTTPS registries.
    pub skip_verify: bool,
    /// Include this host when resolving unqualified image references.
    pub search: bool,
    /// Anonymous, basic, or bearer token authentication.
    pub auth: ImageRegistryAuth,
}
```

#### Example

```rust
use boxlite::{BoxliteOptions, ImageRegistry};
use std::path::PathBuf;

let options = BoxliteOptions {
    home_dir: PathBuf::from("/var/lib/boxlite"),
    image_registries: vec![
        ImageRegistry::https("ghcr.io/myorg").with_search(true),
        ImageRegistry::https("docker.io").with_search(true),
        ImageRegistry::http("registry.local:5000").with_search(true),
        ImageRegistry::https("registry.example.com")
            .with_skip_verify(true)
            .with_basic_auth("user", "password"),
    ],
};
// "alpine" tries ghcr.io/myorg/alpine, then docker.io/alpine,
// then registry.local:5000/library/alpine.
```

---

## Box Handle

### LiteBox

Handle to a box instance. Thin wrapper providing access to box operations.

```rust
pub struct LiteBox {
    // ... internal fields
}
```

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `id` | `fn id(&self) -> &BoxID` | Get box ID |
| `name` | `fn name(&self) -> Option<&str>` | Get optional box name |
| `info` | `async fn info(&self) -> Result<BoxInfo>` | Get box info (no VM init) |
| `network` | `fn network(&self) -> NetworkHandle` | Get box-scoped tunnel operations |
| `start` | `async fn start(&self) -> BoxliteResult<()>` | Start the box |
| `run` | `async fn run(&self, command: BoxCommand) -> BoxliteResult<Execution>` | Run command |
| `metrics` | `async fn metrics(&self) -> BoxliteResult<BoxMetrics>` | Get box metrics |
| `stop` | `async fn stop(&self) -> BoxliteResult<()>` | Stop the box |

#### Lifecycle

- `start()` initializes VM for `Configured` or `Stopped` boxes
- Idempotent: calling on `Running` box is a no-op
- `run()` implicitly calls `start()` if needed
- `stop()` terminates VM; box can be restarted

#### Example

```rust
let litebox = runtime.create(BoxOptions::default(), None).await?;

// Start explicitly (optional, run does this automatically)
litebox.start().await?;

// Check metrics
let metrics = litebox.metrics().await?;
println!("CPU: {:?}%", metrics.cpu_percent());

// Stop when done
litebox.stop().await?;
```

### BoxInfo

Public metadata about a box (returned by list operations).

```rust
pub struct BoxInfo {
    /// Unique box identifier (ULID)
    pub id: BoxID,

    /// User-defined name (optional)
    pub name: Option<String>,

    /// Current lifecycle status
    pub status: BoxStatus,

    /// Creation timestamp (UTC)
    pub created_at: DateTime<Utc>,

    /// Last state change timestamp (UTC)
    pub last_updated: DateTime<Utc>,

    /// Process ID of VMM subprocess (None if not running)
    pub pid: Option<u32>,

    /// Image reference or rootfs path
    pub image: String,

    /// Allocated CPU count
    pub cpus: u8,

    /// Allocated memory in MiB
    pub memory_mib: u32,

    /// Current network configuration and resolved local publications
    pub network: Option<NetworkInfo>,

    /// User-defined labels
    pub labels: HashMap<String, String>,
}

pub struct OutboundNetworkInfo {
// (InboundNetworkInfo has the same shape)
    pub mode: NetworkMode,
    pub allow_net: Vec<String>,
}

pub struct NetworkInfo {
    pub outbound: OutboundNetworkInfo,
    pub inbound: InboundNetworkInfo,
    pub published_ports: Option<Vec<PublishedPort>>,
    // Deprecated mirrors of `outbound`, kept so pre-split readers keep
    // compiling. Build with `NetworkInfo::new` so they cannot disagree.
    pub mode: NetworkMode,
    pub allow_net: Vec<String>,
}

pub struct PublishedPort {
    pub guest_port: u16,
    pub host_ip: String,
    pub host_port: u16,
    pub protocol: PortProtocol,
}
```

`network: None` means network information is unavailable, such as metadata from
an older or remote producer. Within `NetworkInfo`, `published_ports: None`
means the current handle does not know the lifecycle's publications.
`Some(vec![])` means there are no active publications, and a populated vector
contains concrete active local bindings. `PortSpec` remains the request type;
resolved output uses `PublishedPort` so every reported host port is concrete.

`LiteBox::info()` has no synchronous snapshot variant. Local metadata reads report
bindings captured when this handle started or reattached the box; REST metadata
reads fetch the current server record. A newly loaded local running box has no
live binding data yet, so `info()`, `get_info()`, or `list_info()` may report
`published_ports: None` until an operation requiring live state reattaches it.

### BoxStatus

Lifecycle status of a box.

```rust
pub enum BoxStatus {
    /// Cannot determine state (error recovery)
    Unknown,

    /// Created and persisted, VM not started
    Configured,

    /// Running and accepting commands
    Running,

    /// Shutting down gracefully (transient)
    Stopping,

    /// Not running, can be restarted
    Stopped,
}
```

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `is_active` | `fn is_active(&self) -> bool` | True if VM process running |
| `is_running` | `fn is_running(&self) -> bool` | True if Running |
| `is_configured` | `fn is_configured(&self) -> bool` | True if Configured |
| `is_stopped` | `fn is_stopped(&self) -> bool` | True if Stopped |
| `is_transient` | `fn is_transient(&self) -> bool` | True if Stopping |
| `can_start` | `fn can_start(&self) -> bool` | True if Configured or Stopped |
| `can_stop` | `fn can_stop(&self) -> bool` | True if Running |
| `can_remove` | `fn can_remove(&self) -> bool` | True if Configured, Stopped, or Unknown |
| `can_run` | `fn can_run(&self) -> bool` | True if Configured, Running, or Stopped |

#### State Machine

```
create() → Configured (persisted to DB, no VM)
start()  → Running (VM initialized)
stop()   → Stopped (VM terminated, can restart)
```

### BoxState

Dynamic box state (changes during lifecycle).

```rust
pub struct BoxState {
    /// Current lifecycle status
    pub status: BoxStatus,

    /// Process ID (None if not running)
    pub pid: Option<u32>,

    /// Container ID (64-char hex)
    pub container_id: Option<ContainerID>,

    /// Last state change timestamp (UTC)
    pub last_updated: DateTime<Utc>,

    /// Lock ID for multiprocess-safe locking
    pub lock_id: Option<LockId>,
}
```

---

## Network Tunnels

| Operation | Signature | Description |
|-----------|-----------|-------------|
| Get handle | `LiteBox::network(&self) -> NetworkHandle` | Get box-scoped network operations |
| Tunnel | `async fn tunnel(&self, target: SocketAddr) -> BoxliteResult<BoxTunnel>` | Prepare a one-shot tunnel to one guest TCP service |
| Forward | `async fn BoxTunnel::forward(self, listen: SocketAddress) -> BoxliteResult<TunnelForwarder>` | Consume the tunnel into a TCP or Unix listener |
| Inspect | `BoxTunnel::uri(&self) -> Option<&str>` | Read the prepared public URL; `None` for a local box |
| Descriptor | `BoxConnection::raw_fd(&self) -> Option<RawFd>` | Borrowed fd; `None` for a remotely served connection |
| Take fd | `BoxConnection::into_fd(self) -> BoxliteResult<OwnedFd>` | Consume the connection and own its descriptor |
| Connect | `BoxTunnel::connect(self) -> BoxliteResult<BoxConnection>` | Consume the prepared tunnel into its byte stream |
| Split | `BoxConnection::into_split(self) -> (BoxReader, BoxWriter)` | Halves that read and write concurrently |
| Half-close | `BoxWriter::shutdown(&mut self) -> BoxliteResult<()>` | Signal EOF; a peer that already hung up is success, not an error |

`BoxTunnel` remains one-shot: choose either `connect()` or `forward()`. A
forwarder privately prepares a fresh tunnel for each later accepted client.
This differs from `BoxOptions.ports`, which publishes a local port for
the lifetime of the box.

`TunnelForwarder::local_addr()` reports the canonical bound address, while
repeatable `wait()` and `close()` share the listener's cached terminal result.
Dropping the final handle requests cancellation.

---

## Command Execution

### BoxCommand

Command builder for running programs in a box.

```rust
use boxlite::BoxCommand;
use std::time::Duration;

let cmd = BoxCommand::new("python3")
    .args(["-c", "print('hello')"])
    .env("PYTHONPATH", "/app")
    .timeout(Duration::from_secs(30))
    .working_dir("/workspace")
    .user("nobody")
    .tty(true);
```

#### Builder Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `new` | `fn new(command: impl Into<String>) -> Self` | Create command |
| `arg` | `fn arg(self, arg: impl Into<String>) -> Self` | Add single argument |
| `args` | `fn args<I, S>(self, args: I) -> Self` | Add multiple arguments |
| `env` | `fn env(self, key: impl Into<String>, val: impl Into<String>) -> Self` | Set env var |
| `timeout` | `fn timeout(self, timeout: Duration) -> Self` | Set run timeout |
| `working_dir` | `fn working_dir(self, dir: impl Into<String>) -> Self` | Set working directory |
| `user` | `fn user(self, user: impl Into<String>) -> Self` | Set execution user (e.g., `"nobody"`, `"1000:1000"`) |
| `tty` | `fn tty(self, enable: bool) -> Self` | Enable pseudo-terminal |

### Execution

Handle to a running command.

```rust
use boxlite::BoxCommand;
use futures::StreamExt;

let mut run_handle = litebox.run(BoxCommand::new("ls").arg("-la")).await?;

// Read stdout as stream
let mut stdout = run_handle.stdout().unwrap();
while let Some(line) = stdout.next().await {
    println!("{}", line);
}

// Wait for completion
let status = run_handle.wait().await?;
println!("Exit code: {}", status.exit_code);
```

#### Methods

| Method | Signature | Description |
|--------|-----------|-------------|
| `id` | `fn id(&self) -> &ExecutionId` | Get run ID |
| `stdin` | `fn stdin(&mut self) -> Option<ExecStdin>` | Take stdin stream (once) |
| `stdout` | `fn stdout(&mut self) -> Option<ExecStdout>` | Take stdout stream (once) |
| `stderr` | `fn stderr(&mut self) -> Option<ExecStderr>` | Take stderr stream (once) |
| `wait` | `async fn wait(&mut self) -> BoxliteResult<ExecResult>` | Wait for completion |
| `kill` | `async fn kill(&mut self) -> BoxliteResult<()>` | Send SIGKILL |
| `signal` | `async fn signal(&self, signal: i32) -> BoxliteResult<()>` | Send signal |
| `resize_tty` | `async fn resize_tty(&self, rows: u32, cols: u32) -> BoxliteResult<()>` | Resize PTY |

### ExecStdin

Standard input stream (write-only).

```rust
pub struct ExecStdin {
    // ...
}

impl ExecStdin {
    /// Write data to stdin
    pub async fn write(&mut self, data: &[u8]) -> BoxliteResult<()>;

    /// Write all data to stdin
    pub async fn write_all(&mut self, data: &[u8]) -> BoxliteResult<()>;
}
```

#### Example

```rust
let mut run_handle = litebox.run(BoxCommand::new("cat")).await?;

// Get stdin handle
let mut stdin = run_handle.stdin().unwrap();

// Write data
stdin.write(b"Hello from stdin!\n").await?;
stdin.write_all(b"More data\n").await?;

// Drop stdin to close (signals EOF to process)
drop(stdin);

let result = run_handle.wait().await?;
```

### ExecStdout / ExecStderr

Standard output/error streams (read-only). Implements `futures::Stream<Item = String>`.

```rust
use futures::StreamExt;

let mut run_handle = litebox.run(BoxCommand::new("ls")).await?;

// Read stdout
let mut stdout = run_handle.stdout().unwrap();
while let Some(line) = stdout.next().await {
    println!("stdout: {}", line);
}

// Read stderr
let mut stderr = run_handle.stderr().unwrap();
while let Some(line) = stderr.next().await {
    eprintln!("stderr: {}", line);
}
```

#### Concurrent Reading

```rust
use futures::StreamExt;
use tokio::select;

let mut run_handle = litebox.run(BoxCommand::new("my-command")).await?;
let mut stdout = run_handle.stdout().unwrap();
let mut stderr = run_handle.stderr().unwrap();

loop {
    select! {
        Some(line) = stdout.next() => println!("stdout: {}", line),
        Some(line) = stderr.next() => eprintln!("stderr: {}", line),
        else => break,
    }
}
```

### ExecResult

Exit status of a process.

```rust
pub struct ExecResult {
    /// Exit code (0 = success, negative = signal number)
    pub exit_code: i32,
}

impl ExecResult {
    /// Returns true if exit code was 0
    pub fn success(&self) -> bool;

    /// Get exit code
    pub fn code(&self) -> i32;
}
```

---

## Box Configuration

### BoxOptions

Options for constructing a box.

```rust
pub struct BoxOptions {
    /// Number of CPUs (default: 2)
    pub cpus: Option<u8>,

    /// Memory in MiB (default: 512)
    pub memory_mib: Option<u32>,

    /// Disk size in GB for rootfs (sparse, grows as needed)
    pub disk_size_gb: Option<u64>,

    /// Working directory inside box
    pub working_dir: Option<String>,

    /// Environment variables
    pub env: Vec<(String, String)>,

    /// Root filesystem source
    pub rootfs: RootfsSpec,

    /// Volume mounts
    pub volumes: Vec<VolumeSpec>,

    /// Network policy, per direction
    pub network: NetworkSpec,

    /// Inbound reachability of exposed services
    pub inbound_network: NetworkSpec,

    /// Outbound HTTP(S) secret substitution rules
    pub secrets: Vec<Secret>,

    /// Port mappings
    pub ports: Vec<PortSpec>,

    /// Per-direction bandwidth cap for the box's interface, in kilobits/sec.
    /// Local runtime only; remote runtimes reject it.
    pub net_bandwidth: NetBandwidth,

    /// Auto-remove box when stopped (default: true)
    pub auto_remove: bool,

    /// Run independently of parent process (default: false)
    pub detach: bool,

    /// Advanced options for expert users (capabilities, security, mount isolation).
    pub advanced: AdvancedBoxOptions,
}
```

#### Example

```rust
use boxlite::{AdvancedBoxOptions, ContainerCapabilities};
use boxlite::runtime::options::{BoxOptions, RootfsSpec, VolumeSpec, PortSpec};

let options = BoxOptions {
    cpus: Some(4),
    memory_mib: Some(2048),
    rootfs: RootfsSpec::Image("python:3.11".to_string()),
    env: vec![
        ("PYTHONPATH".to_string(), "/app".to_string()),
    ],
    volumes: vec![
        VolumeSpec::bind_mount("/home/user/project", "/app"),
    ],
    ports: vec![
        PortSpec {
            host_port: Some(8080),
            guest_port: 80,
            ..Default::default()
        },
    ],
    advanced: AdvancedBoxOptions {
        capabilities: ContainerCapabilities {
            add: vec!["SYS_ADMIN".to_string()],
            drop: vec!["NET_RAW".to_string()],
        },
        ..Default::default()
    },
    auto_remove: false,  // Keep box after stop
    detach: true,        // Run independently
    ..Default::default()
};
```

### AdvancedBoxOptions

Advanced options for expert users. Most users can ignore this — defaults enable
the isolation protections supported by the host platform.

```rust
pub struct AdvancedBoxOptions {
    pub capabilities: ContainerCapabilities,
    pub security: SecurityOptions,
    pub isolate_mounts: bool,
    pub health_check: Option<HealthCheckOptions>,
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `capabilities` | `ContainerCapabilities` | Empty add/drop lists | Linux capability delta policy for init and exec processes |
| `security` | `SecurityOptions` | `SecurityOptions::default()` (fully enabled profile; jailer enabled) | Security isolation options (jailer, seccomp, namespaces) |
| `isolate_mounts` | `bool` | `false` | Enable bind mount isolation (requires CAP_SYS_ADMIN on Linux) |
| `health_check` | `Option<HealthCheckOptions>` | `None` | Optional guest-agent health monitoring |

### RootfsSpec

How to populate the box root filesystem.

```rust
pub enum RootfsSpec {
    /// Pull/resolve this registry image reference
    Image(String),

    /// Use already prepared rootfs at host path
    RootfsPath(String),
}

impl Default for RootfsSpec {
    fn default() -> Self {
        Self::Image("alpine:latest".into())
    }
}
```

### VolumeSpec

Filesystem mount specification. A mount has exactly one origin: a managed
volume, or a host bind path.

```rust
pub struct VolumeSpec {
    /// Managed volume, by server-assigned id or by name.
    pub managed_volume: Option<String>,

    /// Path on host. Empty when `managed_volume` is set.
    pub host_path: String,

    /// Path inside guest
    pub guest_path: String,

    /// Mount as read-only
    pub read_only: bool,
}
```

The constructor names the operation; the `host_path` field names what it holds.
The field keeps its name because it is persisted box config: boxes on disk carry
a `host_path` key, so renaming the field would strand every box written before
such a rename. Renaming the constructor, as here, does not touch what is stored.

Build one with a constructor rather than by hand:

```rust
use boxlite::runtime::options::VolumeSpec;

// Managed volume, addressed by name — `"vol_01K2EXAMPLE"` works the same way.
let by_name = VolumeSpec::managed_volume("my-data", "/data");

// Host bind mount, read-only.
let bind = VolumeSpec {
    read_only: true,
    ..VolumeSpec::bind_mount("/tmp/data", "/data")
};
```

The reference is taken verbatim and reaches the wire unchanged — nothing
decorates or narrows it.

The two origins are not interchangeable across runtimes:

| Origin | Local runtime | REST runtime |
| --- | --- | --- |
| `VolumeSpec::managed_volume` | rejected — no volume backend | mounted |
| `VolumeSpec::bind_mount` | mounted | rejected — the path is the server's, not yours |

### NetworkSpec

Guest egress policy — unchanged by the outbound/inbound split.

```rust
pub enum NetworkSpec {
    Enabled {
        allow_net: Vec<String>, // empty = full access
    },
    Disabled,                   // no guest network interface at all
}
```

`allow_net` supports exact hosts, wildcard hosts, IPs, and CIDRs, and restricts both TCP and UDP egress. Hostname rules rely on TLS SNI / HTTP Host inspection, which only TCP carries, so an `allow_net` holding only hostnames denies all UDP egress — add the IP or CIDR to keep UDP open. `Disabled` removes the guest network interface entirely.

The inbound direction — whether services the box exposes are reachable from
outside it — is the sibling field `BoxOptions::inbound_network`, which reuses
this same type: `Enabled` = reachable (the default), `Disabled` = private.
The two directions are independent, so a box may refuse egress while the
services it exposes stay reachable, or the reverse.

```rust
let opts = BoxOptions {
    network: NetworkSpec::Disabled,                                  // no egress
    inbound_network: NetworkSpec::Enabled { allow_net: vec![] },     // still reachable
    ..Default::default()
};
```

`inbound_network`'s `allow_net` must be empty — no layer enforces an inbound
allowlist yet, so a non-empty value is rejected at create. Inbound is
controlled by enabled/disabled alone.

Pre-split code needs no change: `network` keeps its name, type and meaning,
and box configs persisted without `inbound_network` load with it defaulted.

### NetBandwidth

Bandwidth cap for the box's network interface, in kilobits per second.

```rust
pub struct NetBandwidth {
    pub tx_kbps: Option<u64>,   // guest -> internet
    pub rx_kbps: Option<u64>,   // internet -> guest
}
```

Directions are named from the box's point of view, matching Firecracker's net
device: `tx` is what the box sends, `rx` is what reaches it. Which side opened
the connection does not matter — traffic arriving over an inbound port forward
is charged to `rx` exactly like a reply to an outbound request. The cap is on
the interface, not on a connection's direction.

Shaping happens below IP in the gvproxy bridge, so one budget per direction
covers TCP, UDP, ICMP and ARP together; there is no per-protocol split.

`None` or `0` in a direction leaves that direction uncapped, the convention
Firecracker, Kata and Cloud Hypervisor share.

```rust
let opts = BoxOptions {
    net_bandwidth: NetBandwidth {
        tx_kbps: Some(10_000),   // 10 Mbit/s up
        rx_kbps: Some(100_000),  // 100 Mbit/s down
    },
    ..Default::default()
};
```

Local runtime only. A remote server owns its own network policy, so the REST
wire types carry no field for this and a remote create rejects it. Setting a cap
while `network` is `Disabled` is also rejected — there is no interface to shape.

**Platform support.** The cap is verified on Linux, where the guest link is a
SOCK_STREAM socket and declining to read it applies backpressure all the way to
the guest's virtio queue. macOS uses a SOCK_DGRAM link whose sender behaviour
under a full receive buffer is not yet verified, so `tx_kbps` there may drop
frames rather than slow the guest down. `rx_kbps` is paced the same way on both.

### Secret

Outbound HTTP(S) secret substitution rule.

```rust
pub struct Secret {
    pub name: String,
    pub hosts: Vec<String>,
    pub placeholder: String, // default: <BOXLITE_SECRET:{name}>
    pub value: String,
}
```

### PortSpec

Port mapping specification (host → guest).

```rust
pub struct PortSpec {
    /// Host port (None/0 = dynamically assigned)
    pub host_port: Option<u16>,

    /// Guest port to expose
    pub guest_port: u16,

    /// Protocol (TCP; UDP is currently rejected)
    pub protocol: PortProtocol,

    /// Bind IP (None = 0.0.0.0)
    pub host_ip: Option<String>,
}

pub enum PortProtocol {
    Tcp,  // default
    Udp,
}
```

---

## Security

### SecurityOptions

Security isolation options for a box.

```rust
pub struct SecurityOptions {
    /// Enable jailer isolation (Linux: seccomp/namespaces, macOS: sandbox-exec)
    pub jailer_enabled: bool,

    /// Enable seccomp syscall filtering (Linux only)
    pub seccomp_enabled: bool,

    /// UID to drop to (Linux only). None = auto-allocate
    pub uid: Option<u32>,

    /// GID to drop to (Linux only). None = auto-allocate
    pub gid: Option<u32>,

    /// Create new PID namespace (Linux only)
    pub new_pid_ns: bool,

    /// Create new network namespace (Linux only)
    pub new_net_ns: bool,

    /// Base directory for chroot jails (Linux)
    pub chroot_base: PathBuf,

    /// Enable chroot isolation (Linux only)
    pub chroot_enabled: bool,

    /// Close inherited file descriptors
    pub close_fds: bool,

    /// Sanitize environment variables
    pub sanitize_env: bool,

    /// Environment variables to preserve
    pub env_allowlist: Vec<String>,

    /// Resource limits
    pub resource_limits: ResourceLimits,

    /// Custom sandbox profile (macOS only)
    pub sandbox_profile: Option<PathBuf>,

    /// Enable network in sandbox (macOS only)
    pub network_enabled: bool,
}
```

#### Settings

Security is a two-state switch — **enable** (the default) or **disable**.

```rust
// Enabled (the default): full host isolation.
let on = SecurityOptions::enabled(); // == SecurityOptions::default()
// - jailer_enabled: true
// - seccomp_enabled, new_pid_ns, chroot_enabled: true (Linux)
// - uid/gid: 65534 (nobody/nogroup)
// - close_fds, sanitize_env: true; resource limits applied

// Disabled: master switch off, every sub-protection off (debugging / unsandboxable envs).
let off = SecurityOptions::disabled();
// - jailer_enabled: false
// - all sub-protections off
```

### SecurityOptionsBuilder

Fluent builder for security options.

```rust
use boxlite::runtime::options::{SecurityOptions, SecurityOptionsBuilder};

let security = SecurityOptionsBuilder::enabled()
    .max_open_files(2048)
    .max_file_size_bytes(1024 * 1024 * 512)  // 512 MiB
    .max_processes(100)
    .allow_env("MY_VAR")
    .build();

// Or via SecurityOptions::builder()
let security = SecurityOptions::builder()
    .seccomp_enabled(false)
    .build();
```

#### Builder Methods

| Method | Description |
|--------|-------------|
| `new()` | Start from defaults |
| `development()` | Start from dev preset |
| `standard()` | Start from standard preset |
| `maximum()` | Start from max preset |
| `jailer_enabled(bool)` | Enable/disable jailer |
| `seccomp_enabled(bool)` | Enable/disable seccomp |
| `uid(u32)` | Set drop-to UID |
| `gid(u32)` | Set drop-to GID |
| `new_pid_ns(bool)` | Enable PID namespace |
| `new_net_ns(bool)` | Enable network namespace |
| `chroot_base(path)` | Set chroot base dir |
| `chroot_enabled(bool)` | Enable chroot |
| `close_fds(bool)` | Close inherited FDs |
| `sanitize_env(bool)` | Sanitize environment |
| `env_allowlist(vec)` | Set env allowlist |
| `allow_env(var)` | Add to env allowlist |
| `resource_limits(limits)` | Set all limits |
| `max_open_files(n)` | RLIMIT_NOFILE |
| `max_file_size_bytes(n)` | RLIMIT_FSIZE |
| `max_processes(n)` | RLIMIT_NPROC |
| `max_memory_bytes(n)` | RLIMIT_AS |
| `max_cpu_time_seconds(n)` | RLIMIT_CPU |
| `sandbox_profile(path)` | macOS sandbox profile |
| `network_enabled(bool)` | macOS network access |
| `build()` | Build SecurityOptions |

### ResourceLimits

Resource limits for the jailed process.

```rust
pub struct ResourceLimits {
    /// Max open file descriptors (RLIMIT_NOFILE)
    pub max_open_files: Option<u64>,

    /// Max file size in bytes (RLIMIT_FSIZE)
    pub max_file_size: Option<u64>,

    /// Max number of processes (RLIMIT_NPROC)
    pub max_processes: Option<u64>,

    /// Max virtual memory in bytes (RLIMIT_AS)
    pub max_memory: Option<u64>,

    /// Max CPU time in seconds (RLIMIT_CPU)
    pub max_cpu_time: Option<u64>,
}
```

---

## Metrics

### RuntimeMetrics

Runtime-wide metrics (aggregate across all boxes).

```rust
let metrics = runtime.metrics().await;
println!("Boxes created: {}", metrics.boxes_created_total());
println!("Commands run: {}", metrics.total_commands_run());
```

#### Methods

| Method | Return | Description |
|--------|--------|-------------|
| `boxes_created_total()` | `u64` | Total boxes created |
| `boxes_failed_total()` | `u64` | Total boxes that failed to start |
| `num_running_boxes()` | `u64` | Currently running boxes |
| `total_commands_run()` | `u64` | Total run() calls |
| `total_run_errors()` | `u64` | Total run errors |

### BoxMetrics

Per-box metrics (individual LiteBox statistics).

```rust
let metrics = litebox.metrics().await?;
println!("Boot time: {:?}ms", metrics.guest_boot_duration_ms());
println!("CPU: {:?}%", metrics.cpu_percent());
println!("Memory: {:?} bytes", metrics.memory_bytes());
```

#### Fields

| Field | Type | Description |
|-------|------|-------------|
| `commands_run_total` | `u64` | Commands on this box |
| `run_errors_total` | `u64` | Run errors on this box |
| `bytes_sent_total` | `u64` | Bytes sent (stdin) |
| `bytes_received_total` | `u64` | Bytes received (stdout/stderr) |
| `total_create_duration_ms` | `Option<u128>` | Total init time |
| `guest_boot_duration_ms` | `Option<u128>` | Guest boot time |
| `cpu_percent` | `Option<f32>` | CPU usage (0-100) |
| `memory_bytes` | `Option<u64>` | Memory usage |
| `network_bytes_sent` | `Option<u64>` | Network TX |
| `network_bytes_received` | `Option<u64>` | Network RX |
| `network_tcp_connections` | `Option<u64>` | Active TCP connections |
| `network_tcp_errors` | `Option<u64>` | TCP connection errors |

#### Stage Timing

| Field | Description |
|-------|-------------|
| `stage_filesystem_setup_ms` | Stage 1: Directory setup |
| `stage_image_prepare_ms` | Stage 2: Image pull/extract |
| `stage_guest_rootfs_ms` | Stage 3: Guest rootfs bootstrap |
| `stage_box_config_ms` | Stage 4: Box config build |
| `stage_box_spawn_ms` | Stage 5: Subprocess spawn |
| `stage_container_init_ms` | Stage 6: Container init |

---

## Type Utilities

### Bytes

Semantic newtype for byte sizes.

```rust
use boxlite::runtime::types::Bytes;

// Constructors
let size = Bytes::from_bytes(1_000_000);
let size = Bytes::from_kib(512);   // 512 * 1024
let size = Bytes::from_mib(128);   // 128 * 1024²
let size = Bytes::from_gib(2);     // 2 * 1024³

// Accessors
let bytes = size.as_bytes();
let kib = size.as_kib();
let mib = size.as_mib();

// Display
println!("{}", Bytes::from_mib(512));  // "512 MiB"
```

### Seconds

Semantic newtype for durations.

```rust
use boxlite::runtime::types::Seconds;

// Constructors
let duration = Seconds::from_seconds(30);
let duration = Seconds::from_minutes(5);   // 300 seconds
let duration = Seconds::from_hours(1);     // 3600 seconds

// Accessors
let secs = duration.as_seconds();
let mins = duration.as_minutes();

// Display
println!("{}", Seconds::from_minutes(30));  // "30 minutes"
```

### BoxID

Box identifier in Base62 format (12 characters, ~71 bits of entropy).

```rust
use boxlite::runtime::id::{BoxID, BoxIDMint};

let id = BoxIDMint::mint();
println!("Full: {}", id.as_str());   // "aB3cD4eF5gH6"
println!("Short: {}", id.short());    // "aB3cD4eF"

// Validation (accepts 12-char Base62 and 26-char legacy ULID)
let valid = BoxID::parse("aB3cD4eF5gH6");
let invalid = BoxID::parse("too-short");  // None
```

### ContainerID

Container identifier (64-char lowercase hex, OCI format).

```rust
use boxlite::runtime::types::ContainerID;

let id = ContainerID::new();
println!("Full: {}", id.as_str());   // 64 hex chars
println!("Short: {}", id.short());    // 12 hex chars

// Validation
let valid = ContainerID::is_valid("a".repeat(64).as_str());  // true
```

---

## Error Types

### BoxliteError

Central error enum for all BoxLite operations.

```rust
pub enum BoxliteError {
    /// Unsupported engine kind
    UnsupportedEngine,

    /// Engine reported an error
    Engine(String),

    /// Configuration error
    Config(String),

    /// Storage/filesystem error
    Storage(String),

    /// Image pull/resolve error
    Image(String),

    /// Host-guest communication error
    Portal(String),

    /// Network error
    Network(String),

    /// gRPC error
    Rpc(String),

    /// gRPC transport error
    RpcTransport(String),

    /// Internal error
    Internal(String),

    /// Command run error
    Run(String),

    /// Unsupported operation
    Unsupported(String),

    /// Box not found
    NotFound(String),

    /// Resource already exists
    AlreadyExists(String),

    /// Invalid state for operation
    InvalidState(String),

    /// Database error
    Database(String),

    /// Metadata parsing error
    MetadataError(String),

    /// Invalid argument
    InvalidArgument(String),
}
```

### BoxliteResult

Result type alias for BoxLite operations.

```rust
pub type BoxliteResult<T> = Result<T, BoxliteError>;
```

#### Error Handling Example

```rust
use boxlite::BoxliteError;

match runtime.create(options, None).await {
    Ok(litebox) => println!("Created box: {}", litebox.id()),
    Err(BoxliteError::Image(msg)) => eprintln!("Image error: {}", msg),
    Err(BoxliteError::Config(msg)) => eprintln!("Config error: {}", msg),
    Err(e) => eprintln!("Other error: {}", e),
}
```

---

## Complete Example

```rust
use boxlite::runtime::{BoxliteRuntime, BoxOptions};
use boxlite::runtime::options::{AdvancedBoxOptions, RootfsSpec, SecurityOptions, VolumeSpec};
use boxlite::BoxCommand;
use futures::StreamExt;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize runtime
    let runtime = BoxliteRuntime::with_defaults()?;

    // Configure box
    let options = BoxOptions {
        cpus: Some(2),
        memory_mib: Some(1024),
        rootfs: RootfsSpec::Image("python:3.11-slim".to_string()),
        volumes: vec![
            VolumeSpec {
                read_only: true,
                ..VolumeSpec::bind_mount("/home/user/code", "/app")
            },
        ],
        advanced: AdvancedBoxOptions {
            security: SecurityOptions::enabled(),
            ..Default::default()
        },
        ..Default::default()
    };

    // Create and name the box
    let litebox = runtime.create(options, Some("python-sandbox".to_string())).await?;
    println!("Created box: {}", litebox.id());

    // Run Python code
    let cmd = BoxCommand::new("python3")
        .args(["-c", "import sys; print(f'Python {sys.version}')"])
        .timeout(Duration::from_secs(30))
        .working_dir("/app");

    let mut run_handle = litebox.run(cmd).await?;

    // Stream output
    if let Some(mut stdout) = run_handle.stdout() {
        while let Some(line) = stdout.next().await {
            println!("{}", line);
        }
    }

    // Check result
    let result = run_handle.wait().await?;
    if !result.success() {
        eprintln!("Command failed with exit code: {}", result.exit_code);
    }

    // Check metrics
    let metrics = litebox.metrics().await?;
    if let Some(boot_ms) = metrics.guest_boot_duration_ms() {
        println!("Boot time: {}ms", boot_ms);
    }

    // Cleanup
    litebox.stop().await?;

    Ok(())
}
```

---

## Thread Safety

All public types are `Send + Sync`:

- `BoxliteRuntime` - safely shareable across threads
- `LiteBox` - safely shareable across threads
- `Execution` - Clone + shareable

```rust
use std::sync::Arc;
use tokio::task;

let runtime = Arc::new(BoxliteRuntime::with_defaults()?);

let handles: Vec<_> = (0..4).map(|i| {
    let rt = runtime.clone();
    task::spawn(async move {
        let box_opts = BoxOptions::default();
        let litebox = rt.create(box_opts, None).await?;
        // Each task has its own box
        Ok::<_, BoxliteError>(litebox.id().clone())
    })
}).collect();

for handle in handles {
    let id = handle.await??;
    println!("Created: {}", id);
}
```

---

## See Also

- [Getting Started Guide](../../getting-started/README.md)
- [Architecture Overview](../../architecture/README.md)
- [Configuration Reference](../README.md)
- [Python SDK Reference](../python/README.md)
- [Node.js SDK Reference](../nodejs/README.md)
