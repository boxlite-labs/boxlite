# Reference Documentation

Complete reference for BoxLite APIs, configuration, and error handling.

## API Reference

### Python API

For complete Python API documentation, see **[Python SDK README](../../sdks/python/README.md)**.

**Quick Reference:**

| Class/Function | Description |
|----------------|-------------|
| `boxlite.Boxlite` | Main runtime for creating and managing boxes |
| `boxlite.BoxOptions` | Configuration options for creating a box |
| `boxlite.Box` | Handle to a running or stopped box |
| `boxlite.Execution` | Represents a running command execution |
| `boxlite.SimpleBox` | Context manager for basic execution |
| `boxlite.CodeBox` | Specialized box for Python code execution |
| `boxlite.BrowserBox` | Box configured for browser automation |
| `boxlite.ComputerBox` | Box with desktop automation capabilities |
| `boxlite.InteractiveBox` | Box for interactive shell sessions |

See [Python SDK Reference](../../sdks/python/README.md#core-api-reference) for detailed API documentation.

### Rust API

**Core Types:**

```rust
use boxlite::{
    BoxliteRuntime,  // Main runtime
    BoxOptions,       // Box configuration
    LiteBox,          // Box handle
    BoxCommand,       // Command builder
    RootfsSpec,       // Rootfs specification (Image or Directory)
    NetworkSpec,      // Network configuration
    VolumeSpec,       // Volume mount specification
    PortSpec,         // Port forwarding specification
};
```

**Runtime Creation:**

```rust
// Default runtime (~/.boxlite)
let runtime = BoxliteRuntime::default_runtime();

// Custom runtime
let runtime = BoxliteRuntime::new(RuntimeOptions {
    home_dir: Some("/custom/path".into()),
    ..Default::default()
})?;
```

**Box Creation:**

```rust
let options = BoxOptions {
    rootfs: RootfsSpec::Image("alpine:latest".into()),
    cpus: Some(2),
    memory_mib: Some(1024),
    working_dir: Some("/app".into()),
    env: vec![("KEY".into(), "value".into())],
    volumes: vec![VolumeSpec {
        host_path: "/host/data".into(),
        guest_path: "/mnt/data".into(),
        read_only: true,
    }],
    ports: vec![PortSpec {
        host_port: 8080,
        guest_port: 80,
        protocol: Protocol::Tcp,
    }],
    ..Default::default()
};

let (box_id, litebox) = runtime.create(options)?;
```

**Command Execution:**

```rust
use futures::StreamExt;

// Execute command
let mut execution = litebox
    .exec(BoxCommand::new("echo").arg("Hello"))
    .await?;

// Stream stdout
let mut stdout = execution.stdout().unwrap();
while let Some(line) = stdout.next().await {
    println!("{}", line);
}

// Wait for exit
let exit_code = execution.wait().await?;
```

## Configuration Reference

### BoxOptions Parameters

Complete reference for box configuration options.

#### `image: String`

OCI image URI to use for the box rootfs.

**Format:** `[registry/]repository[:tag]`

**Default:** `"python:slim"` (for Python SDK convenience wrappers)

**Examples:**
```python
# Docker Hub (default registry)
image="python:3.11-slim"
image="alpine:latest"
image="ubuntu:22.04"

# GitHub Container Registry
image="ghcr.io/owner/repo:tag"

# Amazon ECR
image="123456.dkr.ecr.us-east-1.amazonaws.com/repo:tag"

# Google Container Registry
image="gcr.io/project/image:tag"
```

**Notes:**
- Images are pulled on first use and cached in `~/.boxlite/images/`
- Layer-level caching for fast subsequent starts
- Authentication: Use registry-specific auth (Docker credentials, etc.)

#### `cpus: int`

Number of CPU cores allocated to the box.

**Default:** 1

**Range:** 1 to host CPU count

**Example:**
```python
cpus=2  # 2 CPU cores
cpus=4  # 4 CPU cores
```

**Notes:**
- CPU scheduling is proportional (shares-based)
- Does not reserve physical cores, just scheduling weight
- Monitor actual usage with `box.metrics().cpu_time_ms`

#### `memory_mib: int`

Memory limit in mebibytes (MiB).

**Default:** 512

**Range:** 128 to 65536 (64 GiB)

**Example:**
```python
memory_mib=1024   # 1 GB
memory_mib=2048   # 2 GB
memory_mib=4096   # 4 GB
```

**Notes:**
- 1 MiB = 1024 KiB = 1,048,576 bytes
- Minimum 128 MiB required for most images
- Out of memory kills the box process
- Monitor with `box.metrics().memory_usage_bytes`

#### `disk_size_gb: int | None`

Create a persistent QCOW2 disk image.

**Default:** `None` (ephemeral storage only)

**Range:** 1 to 1024 (1 TB)

**Example:**
```python
disk_size_gb=None   # Ephemeral (default)
disk_size_gb=10     # 10 GB persistent disk
disk_size_gb=100    # 100 GB persistent disk
```

**Notes:**
- Disk persists across stop/restart
- Stored at `~/.boxlite/boxes/{box-id}/disk.qcow2`
- Copy-on-write (thin provisioned)
- Deleted when box is removed

#### `working_dir: str`

Working directory for command execution inside the box.

**Default:** `"/root"`

**Example:**
```python
working_dir="/app"
working_dir="/home/user/project"
```

**Notes:**
- Directory must exist in the container image
- Commands execute with this as `$PWD`

#### `env: List[Tuple[str, str]]`

Environment variables as (key, value) pairs.

**Default:** `[]` (inherit from image)

**Example:**
```python
env=[
    ("DATABASE_URL", "postgresql://localhost/db"),
    ("API_KEY", "secret"),
    ("DEBUG", "true"),
    ("PATH", "/custom/bin:/usr/bin:/bin"),  # Override PATH
]
```

**Notes:**
- Variables are appended to image environment
- Use to override image defaults (e.g., `PATH`)
- Sensitive values (API keys, passwords) are visible in box metadata

#### `volumes: List[Tuple[str, str, str]]`

Volume mounts as (host_path, guest_path, mode) tuples.

**Format:** `(host_path, guest_path, "ro"|"rw")`

**Default:** `[]` (no mounts)

**Example:**
```python
volumes=[
    # Read-only mount (data input)
    ("/host/config", "/etc/app/config", "ro"),

    # Read-write mount (data output)
    ("/host/data", "/mnt/data", "rw"),

    # Home directory mount
    (os.path.expanduser("~/Documents"), "/mnt/docs", "ro"),
]
```

**Notes:**
- Uses virtiofs for high-performance file sharing
- Host path must exist before box creation
- Guest path is created automatically if missing
- Changes to `rw` mounts are visible on host immediately

#### `ports: List[Tuple[int, int, str]]`

Port forwarding as (host_port, guest_port, protocol) tuples.

**Format:** `(host_port, guest_port, "tcp"|"udp")`

**Default:** `[]` (no port forwarding)

**Example:**
```python
ports=[
    (8080, 80, "tcp"),      # HTTP
    (8443, 443, "tcp"),     # HTTPS
    (5432, 5432, "tcp"),    # PostgreSQL
    (53, 53, "udp"),        # DNS
    (3000, 8000, "tcp"),    # Custom mapping
]
```

**Notes:**
- Uses gvproxy for NAT port mapping
- Host port must be available (not in use)
- Multiple boxes can forward to same host port (error if conflict)
- Supports both TCP and UDP protocols

#### `auto_remove: bool`

Automatically remove box when stopped.

**Default:** `True`

**Example:**
```python
auto_remove=True   # Auto cleanup (default)
auto_remove=False  # Manual cleanup required
```

**Notes:**
- `True`: Box is removed when context exits or `stop()` is called
- `False`: Box persists after stop, can be restarted with `runtime.get(box_id)`
- Manual cleanup: `await box.remove()`

### Runtime Options

#### `home_dir: str`

Base directory for BoxLite runtime data.

**Default:** `~/.boxlite`

**Override:** Set `BOXLITE_HOME` environment variable

**Structure:**
```
~/.boxlite/
├── images/       # OCI image cache (blobs, index.json)
├── boxes/        # Per-box data (config.json, disk.qcow2)
├── init/         # Shared guest rootfs
├── logs/         # Runtime logs
├── gvproxy/      # Network backend binaries
├── lock          # Filesystem lock file
└── db/           # SQLite databases (boxes.db, images.db)
```

**Example:**
```python
# Custom home directory
runtime = boxlite.Boxlite(boxlite.Options(home_dir="/custom/path"))
```

### Environment Variables

#### `BOXLITE_HOME`

Override default runtime home directory.

**Default:** `~/.boxlite`

**Example:**
```bash
export BOXLITE_HOME=/custom/boxlite
python script.py
```

#### `RUST_LOG`

Enable debug logging for troubleshooting.

**Levels:** `trace`, `debug`, `info`, `warn`, `error`

**Example:**
```bash
# Debug logging
RUST_LOG=debug python script.py

# Trace logging (very verbose)
RUST_LOG=trace python script.py

# Module-specific logging
RUST_LOG=boxlite::runtime=debug python script.py
```

#### `BOXLITE_TMPDIR`

Override temporary directory for BoxLite operations.

**Default:** System temp directory (`/tmp` on Linux/macOS)

**Example:**
```bash
export BOXLITE_TMPDIR=/custom/tmp
python script.py
```

## Error Codes & Handling

### Error Types

BoxLite uses a centralized error enum with specific variants for different error categories.

#### `UnsupportedEngine`

Platform or hypervisor not supported.

**Cause:**
- Running on Windows
- Running on Intel Mac
- KVM not available on Linux
- Hypervisor.framework not available on macOS

**Example:**
```
Error: unsupported engine kind
```

**Solution:**
- Use supported platform (macOS ARM64, Linux x86_64/ARM64)
- Verify hypervisor availability:
  - Linux: `grep -E 'vmx|svm' /proc/cpuinfo`
  - macOS: Ensure macOS 12+ on Apple Silicon

#### `Engine(String)`

Hypervisor or VM engine error.

**Cause:**
- KVM module not loaded
- Insufficient permissions for `/dev/kvm`
- Hypervisor.framework error
- VM creation failed

**Example:**
```
Error: engine reported an error: KVM is not available
```

**Solution:**
```bash
# Linux: Load KVM module
sudo modprobe kvm kvm_intel  # or kvm_amd

# Linux: Check /dev/kvm permissions
ls -l /dev/kvm
sudo chmod 666 /dev/kvm

# Linux: Add user to kvm group
sudo usermod -aG kvm $USER
# (logout and login required)
```

#### `Config(String)`

Invalid box configuration.

**Cause:**
- Invalid CPU count (< 1 or > host CPUs)
- Invalid memory size (< 128 or > 65536)
- Invalid paths in volumes
- Invalid port numbers

**Example:**
```
Error: configuration error: CPU count must be between 1 and 8
```

**Solution:**
- Verify configuration parameters are within valid ranges
- Check file paths exist for volume mounts
- Ensure port numbers are valid (1-65535)

#### `Storage(String)`

Filesystem or disk operation error.

**Cause:**
- Disk full (`~/.boxlite` partition)
- Permission denied writing to `~/.boxlite`
- Disk image creation failed
- QCOW2 operation failed

**Example:**
```
Error: storage error: No space left on device
```

**Solution:**
```bash
# Check disk space
df -h ~/.boxlite

# Check permissions
ls -ld ~/.boxlite
chmod 755 ~/.boxlite

# Clean up old boxes
# (manually remove ~/.boxlite/boxes/*)
```

#### `Image(String)`

OCI image pull or extraction error.

**Cause:**
- Network connectivity issues
- Invalid image name or tag
- Registry authentication required
- Image not found in registry
- Corrupted image layers

**Example:**
```
Error: images error: failed to pull image: 404 Not Found
```

**Solution:**
```bash
# Verify image exists
docker pull <image>

# Check network connectivity
ping registry-1.docker.io

# Authenticate for private images
docker login

# Clear image cache if corrupted
rm -rf ~/.boxlite/images/*
```

#### `Portal(String)`

Host-guest communication error (gRPC over vsock).

**Cause:**
- Guest agent not responding
- vsock connection failed
- gRPC timeout
- Guest initialization failed

**Example:**
```
Error: portal error: connection timeout
```

**Solution:**
- Enable debug logging: `RUST_LOG=debug`
- Check if box is running: `box.info().status`
- Restart box: `box.stop()` and recreate
- Report issue with logs if persists

#### `Network(String)`

Network configuration or connectivity error.

**Cause:**
- gvproxy not running or crashed
- Port already in use
- Network backend initialization failed

**Example:**
```
Error: network error: bind: address already in use
```

**Solution:**
```bash
# Check port availability
lsof -i :8080

# Stop conflicting process or use different port
ports=[(8081, 80, "tcp")]

# Verify gvproxy binary exists
ls ~/.boxlite/gvproxy/
```

#### `Execution(String)`

Command execution error.

**Cause:**
- Command not found in image
- Command crashed or killed
- Execution timeout
- Streaming I/O error

**Example:**
```
Error: Execution error: command not found: python3
```

**Solution:**
- Verify command exists in image:
  ```python
  result = await box.exec("which", "python3")
  ```
- Check exit code and stderr:
  ```python
  result = await box.exec("command")
  if result.exit_code != 0:
      print(f"Failed: {result.stderr}")
  ```

#### `Internal(String)`

Internal BoxLite error.

**Cause:**
- Unexpected internal state
- I/O error
- JSON parsing error
- Unhandled edge case

**Example:**
```
Error: internal error: unexpected state transition
```

**Solution:**
- Enable debug logging: `RUST_LOG=debug python script.py`
- Report issue with full logs to GitHub
- Include BoxLite version, platform, and reproduction steps

#### `NotFound(String)`

Box or resource not found.

**Cause:**
- Box ID doesn't exist
- Box was removed
- Image not in cache

**Example:**
```
Error: box not found: 01JJNH8...
```

**Solution:**
- List all boxes: `runtime.list()`
- Verify box ID is correct
- Create new box if needed

#### `AlreadyExists(String)`

Box or resource already exists.

**Cause:**
- Duplicate box creation attempt
- Port already forwarded

**Example:**
```
Error: already exists: box with this ID exists
```

**Solution:**
- Use existing box: `runtime.get(box_id)`
- Remove existing box: `box.remove()`
- Use different configuration (e.g., different port)

#### `InvalidState(String)`

Box is in wrong state for requested operation.

**Cause:**
- Executing command on stopped box
- Stopping already stopped box
- Restarting box that never started

**Example:**
```
Error: invalid state: cannot execute on stopped box
```

**Solution:**
- Check box status: `info = await box.info(); print(info.status)`
- Restart box if stopped: `runtime.get(box_id)` (may auto-restart)
- Create new box if needed

### Error Handling Patterns

#### Python

```python
import boxlite

async def safe_execution():
    try:
        async with boxlite.SimpleBox(image="python:slim") as box:
            result = await box.exec("python", "script.py")

            # Check exit code
            if result.exit_code != 0:
                print(f"Command failed: {result.stderr}")
                return

    except Exception as e:
        # All BoxLite errors are raised as Python exceptions
        print(f"Error: {e}")

        # Enable debug logging for details
        # RUST_LOG=debug python script.py
```

#### Rust

```rust
use boxlite::{BoxliteRuntime, BoxliteError, BoxliteResult};

fn main() -> BoxliteResult<()> {
    let runtime = BoxliteRuntime::default_runtime();

    match runtime.create(options) {
        Ok((box_id, litebox)) => {
            // Success
        }
        Err(BoxliteError::UnsupportedEngine) => {
            eprintln!("Platform not supported");
            return Err(BoxliteError::UnsupportedEngine);
        }
        Err(BoxliteError::Image(msg)) => {
            eprintln!("Image error: {}", msg);
            // Handle image-specific error
        }
        Err(e) => {
            eprintln!("Unexpected error: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
```

## File Formats

### QCOW2 Disk Images

BoxLite uses QCOW2 (QEMU Copy-On-Write version 2) for persistent disks.

**Location:** `~/.boxlite/boxes/{box-id}/disk.qcow2`

**Format:** QCOW2 (copy-on-write image format)

**Features:**
- Thin provisioning (sparse allocation)
- Copy-on-write snapshots
- Compression support

**Size:** Specified by `disk_size_gb` parameter

**Lifecycle:**
- Created on first box start (if `disk_size_gb` set)
- Persists across stop/restart
- Deleted when box is removed

**Tools:**
```bash
# Inspect QCOW2 image
qemu-img info ~/.boxlite/boxes/{box-id}/disk.qcow2

# Convert to raw (if needed)
qemu-img convert -f qcow2 -O raw disk.qcow2 disk.raw
```

### OCI Image Cache

BoxLite caches OCI images at the layer level for fast starts.

**Location:** `~/.boxlite/images/`

**Structure:**
```
~/.boxlite/images/
├── blobs/
│   └── sha256/
│       ├── abc123...  # Layer blob
│       ├── def456...  # Layer blob
│       └── ...
└── index.json         # Image index
```

**Format:** OCI Image Layout Specification

**Caching:**
- Blob-level deduplication across images
- Shared base layers (e.g., `python:3.11` and `python:3.12` share layers)
- Automatic garbage collection (future feature)

**Clearing Cache:**
```bash
# Clear all cached images
rm -rf ~/.boxlite/images/*

# Images will be re-pulled on next use
```

### Box Configuration

Box metadata and state are stored as JSON.

**Location:** `~/.boxlite/boxes/{box-id}/config.json`

**Format:** JSON

**Contents:**
- Box ID (ULID)
- Image specification
- Resource limits (CPUs, memory)
- Volume mounts
- Port forwarding
- Environment variables
- Creation timestamp
- Status (running, stopped, etc.)

**Example:**
```json
{
  "id": "01JJNH8...",
  "image": "python:slim",
  "cpus": 2,
  "memory_mib": 1024,
  "volumes": [
    {
      "host_path": "/host/data",
      "guest_path": "/mnt/data",
      "read_only": true
    }
  ],
  "ports": [
    {
      "host_port": 8080,
      "guest_port": 80,
      "protocol": "tcp"
    }
  ],
  "created_at": "2025-01-15T10:30:00Z",
  "status": "running"
}
```

**Notes:**
- Do not manually edit (managed by BoxLite)
- Used for box persistence and recovery
- Deleted when box is removed

### SQLite Databases

BoxLite uses SQLite for metadata persistence.

**Locations:**
- `~/.boxlite/db/boxes.db` - Box registry and metadata
- `~/.boxlite/db/images.db` - Image cache index

**Schema:**
- Follows Podman-style pattern: immutable config + mutable state
- Box config stored as JSON blob
- Box state tracked separately
- Image layers indexed by digest

**Tools:**
```bash
# Inspect database
sqlite3 ~/.boxlite/db/boxes.db ".tables"
sqlite3 ~/.boxlite/db/boxes.db "SELECT * FROM boxes;"

# Backup
cp ~/.boxlite/db/boxes.db ~/backup/boxes.db.backup
```

**Notes:**
- Do not manually modify (data corruption risk)
- Backed up automatically on box operations
- Corruption recovery: Delete and recreate from box config files
