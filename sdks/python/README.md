# BoxLite Python SDK

Python bindings for BoxLite - an embeddable virtual machine runtime for secure, isolated code execution.

## Overview

The BoxLite Python SDK provides a Pythonic API for creating and managing isolated execution environments. Built with PyO3, it wraps the Rust BoxLite runtime with async-first Python bindings.

**Version:** 0.4.4
**Python:** 3.10+
**Platforms:** macOS (Apple Silicon), Linux (x86_64, ARM64)

### Key Features

- **Async-first API** - All I/O operations use async/await
- **Context managers** - Automatic cleanup with `async with`
- **Streaming I/O** - Real-time stdout/stderr as execution happens
- **Multiple box types** - SimpleBox, CodeBox, BrowserBox, ComputerBox, InteractiveBox
- **Resource control** - Configure CPUs, memory, volumes, ports
- **OCI compatible** - Use any Docker/OCI image

## Installation

```bash
pip install boxlite
```

Requires Python 3.10 or later.

### Verify Installation

```python
import boxlite
print(boxlite.__version__)  # Should print: 0.4.4
```

### System Requirements

| Platform | Architecture  | Requirements                        |
|----------|---------------|-------------------------------------|
| macOS    | Apple Silicon | macOS 12+                           |
| Linux    | x86_64, ARM64 | KVM enabled (`/dev/kvm` accessible) |

On Linux, verify KVM is available:
```bash
grep -E 'vmx|svm' /proc/cpuinfo  # Should show CPU virtualization support
ls -l /dev/kvm                    # Should exist and be accessible
```

## Quick Start

### Basic Execution

```python
import asyncio
import boxlite

async def main():
    # Create a box and run a command
    async with boxlite.SimpleBox(image="python:slim") as box:
        result = await box.exec("python", "-c", "print('Hello from BoxLite!')")
        print(result.stdout)
        # Output: Hello from BoxLite!

asyncio.run(main())
```

### Code Execution (AI Agents)

```python
import asyncio
import boxlite

async def main():
    # Execute untrusted Python code safely
    code = """
import requests
response = requests.get('https://api.github.com/zen')
print(response.text)
"""

    async with boxlite.CodeBox() as codebox:
        # CodeBox automatically installs packages
        result = await codebox.run(code)
        print(result)

asyncio.run(main())
```

## Core API Reference

### Runtime Management

#### `boxlite.Boxlite`

The main runtime for creating and managing boxes.

**Methods:**

- `Boxlite.default() -> Boxlite`
  Create runtime with default settings (`~/.boxlite`)

- `Boxlite(options: Options) -> Boxlite`
  Create runtime with custom options

- `create(box_options: BoxOptions) -> Box`
  Create a new box with specified configuration

- `get(box_id: str) -> Box`
  Reattach to an existing box by ID

- `list() -> List[BoxInfo]`
  List all boxes (running and stopped)

- `metrics() -> RuntimeMetrics`
  Get runtime-wide metrics

**Example:**

```python
# Default runtime
runtime = boxlite.Boxlite.default()

# Custom runtime with different home directory
runtime = boxlite.Boxlite(boxlite.Options(home_dir="/custom/path"))

# Create a box
box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))

# Reattach to existing box
box = runtime.get("01JJNH8...")

# List all boxes
boxes = runtime.list()
for info in boxes:
    print(f"{info.id}: {info.status}")
```

### Box Configuration

#### `boxlite.BoxOptions`

Configuration options for creating a box.

**Parameters:**

- `image: str` - OCI image URI (default: `"python:slim"`)
- `cpus: int` - Number of CPUs (default: 1, max: host CPU count)
- `memory_mib: int` - Memory in MiB (default: 512, range: 128-65536)
- `disk_size_gb: int | None` - Persistent disk size in GB (default: None)
- `working_dir: str` - Working directory in container (default: `"/root"`)
- `env: List[Tuple[str, str]]` - Environment variables as (key, value) pairs
- `volumes: List[Tuple[str, str, str]]` - Volume mounts as (host_path, guest_path, mode)
  - Mode: `"ro"` (read-only) or `"rw"` (read-write)
- `ports: List[Tuple[int, int, str]]` - Port forwarding as (host_port, guest_port, protocol)
  - Protocol: `"tcp"` or `"udp"`
- `auto_remove: bool` - Auto cleanup after stop (default: True)

**Example:**

```python
options = boxlite.BoxOptions(
    image="postgres:latest",
    cpus=2,
    memory_mib=1024,
    disk_size_gb=10,  # 10 GB persistent disk
    env=[
        ("POSTGRES_PASSWORD", "secret"),
        ("POSTGRES_DB", "mydb"),
    ],
    volumes=[
        ("/host/data", "/mnt/data", "ro"),  # Read-only mount
    ],
    ports=[
        (5432, 5432, "tcp"),  # PostgreSQL
    ],
)
box = runtime.create(options)
```

### Box Handle

#### `boxlite.Box`

Handle to a running or stopped box.

**Properties:**

- `id: str` - Unique box identifier (ULID format)

**Methods:**

- `exec(*args, **kwargs) -> Execution`
  Execute a command in the box (async)

- `stop() -> None`
  Stop the box gracefully (async)

- `remove() -> None`
  Delete the box and its data (async)

- `info() -> BoxInfo`
  Get box metadata (async)

- `metrics() -> BoxMetrics`
  Get box resource usage metrics (async)

**Example:**

```python
box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))

# Execute commands
execution = await box.exec("echo", "Hello")
result = await execution.wait()

# Get box info
info = await box.info()
print(f"Box {info.id}: {info.status}")

# Stop and remove
await box.stop()
await box.remove()
```

### Command Execution

#### `boxlite.Execution`

Represents a running command execution.

**Methods:**

- `stdout() -> ExecStdout`
  Get stdout stream (async iterator)

- `stderr() -> ExecStderr`
  Get stderr stream (async iterator)

- `stdin() -> ExecStdin`
  Get stdin writer

- `wait() -> ExecResult`
  Wait for command to complete and get result (async)

- `kill(signal: int = 9) -> None`
  Send signal to process (async)

**Example:**

```python
# Streaming output
execution = await box.exec("python", "-c", "for i in range(5): print(i)")

stdout = execution.stdout()
async for line in stdout:
    print(f"Output: {line}")

# Wait for completion
result = await execution.wait()
print(f"Exit code: {result.exit_code}")
```

#### `boxlite.ExecStdout` / `boxlite.ExecStderr`

Async iterators for streaming output.

**Usage:**

```python
execution = await box.exec("ls", "-la")

# Stream stdout line by line
stdout = execution.stdout()
async for line in stdout:
    print(line)

# Stream stderr
stderr = execution.stderr()
async for line in stderr:
    print(f"Error: {line}", file=sys.stderr)
```

### Higher-Level APIs

#### `boxlite.SimpleBox`

Context manager for basic execution with automatic cleanup.

**Parameters:** Same as `BoxOptions`

**Methods:**

- `exec(*args, **kwargs) -> ExecResult`
  Execute command and wait for result

**Example:**

```python
async with boxlite.SimpleBox(image="python:slim") as box:
    result = await box.exec("python", "-c", "print('Hello')")
    print(result.stdout)  # "Hello\n"
    print(result.exit_code)  # 0
```

#### `boxlite.CodeBox`

Specialized box for Python code execution with package management.

**Methods:**

- `run(code: str) -> str`
  Execute Python code and return output

- `install_package(package: str) -> None`
  Install a Python package with pip

**Example:**

```python
async with boxlite.CodeBox() as codebox:
    # Install packages
    await codebox.install_package("requests")

    # Run code
    result = await codebox.run("""
import requests
print(requests.get('https://api.github.com/zen').text)
""")
    print(result)
```

#### `boxlite.BrowserBox`

Box configured for browser automation (Chromium, Firefox, WebKit).

**Example:**

```python
async with boxlite.BrowserBox() as browser:
    endpoint = browser.endpoint()
    print(f"Connect Puppeteer to: {endpoint}")
    # Use with Puppeteer/Playwright for browser automation
```

#### `boxlite.ComputerBox`

Box with desktop automation capabilities (mouse, keyboard, screenshots).

**Methods:**

14 desktop interaction functions including:
- `screenshot() -> bytes` - Capture screen
- `left_click()` - Click mouse
- `type_text(text: str)` - Type text
- `get_screen_size() -> Tuple[int, int]` - Get screen dimensions

**Example:**

```python
async with boxlite.ComputerBox() as computer:
    # Get screen size
    width, height = await computer.get_screen_size()

    # Take screenshot
    screenshot_bytes = await computer.screenshot()

    # Mouse and keyboard
    await computer.left_click()
    await computer.type_text("Hello, world!")
```

#### `boxlite.InteractiveBox`

Box for interactive shell sessions.

**Example:**

```python
async with boxlite.InteractiveBox(image="alpine:latest") as itbox:
    # Drop into interactive shell
    await itbox.wait()
```

## API Patterns

### Async/Await

All I/O operations are async. Use `await` for operations and `async for` for streams.

```python
# Create and use box (async)
async with boxlite.SimpleBox(image="alpine") as box:
    result = await box.exec("echo", "Hello")

# Stream output (async iterator)
execution = await box.exec("python", "script.py")
async for line in execution.stdout():
    print(line)
```

### Context Managers

Use `async with` for automatic cleanup:

```python
# SimpleBox - auto cleanup
async with boxlite.SimpleBox() as box:
    result = await box.exec("command")
# Box automatically stopped and removed

# Manual cleanup (if not using context manager)
box = runtime.create(boxlite.BoxOptions(image="alpine"))
try:
    await box.exec("command")
finally:
    await box.stop()
    await box.remove()
```

### Streaming I/O

Stream output line-by-line as it's produced:

```python
execution = await box.exec("tail", "-f", "/var/log/app.log")

# Process output in real-time
stdout = execution.stdout()
async for line in stdout:
    if "ERROR" in line:
        print(f"Alert: {line}")
```

### Error Handling

Catch exceptions from BoxLite operations:

```python
import boxlite
from boxlite import BoxliteError, ExecError

try:
    async with boxlite.SimpleBox(image="invalid:image") as box:
        result = await box.exec("command")
except BoxliteError as e:
    print(f"BoxLite error: {e}")
except ExecError as e:
    print(f"Execution error: {e}")
```

## Configuration Reference

### Image Selection

Any OCI-compatible image from Docker Hub, GHCR, ECR, or other registries:

```python
# Docker Hub (default registry)
boxlite.BoxOptions(image="python:3.11-slim")
boxlite.BoxOptions(image="alpine:latest")
boxlite.BoxOptions(image="ubuntu:22.04")

# GitHub Container Registry
boxlite.BoxOptions(image="ghcr.io/owner/repo:tag")

# Amazon ECR
boxlite.BoxOptions(image="123456.dkr.ecr.us-east-1.amazonaws.com/repo:tag")
```

### Resource Limits

```python
boxlite.BoxOptions(
    cpus=4,           # 4 CPU cores
    memory_mib=2048,  # 2 GB RAM
)
```

### Environment Variables

```python
boxlite.BoxOptions(
    env=[
        ("DATABASE_URL", "postgresql://localhost/db"),
        ("API_KEY", "secret"),
        ("DEBUG", "true"),
    ]
)
```

### Volume Mounts

```python
boxlite.BoxOptions(
    volumes=[
        # Read-only mount
        ("/host/config", "/etc/app/config", "ro"),

        # Read-write mount
        ("/host/data", "/mnt/data", "rw"),
    ]
)
```

### Port Forwarding

```python
boxlite.BoxOptions(
    ports=[
        (8080, 80, "tcp"),      # HTTP
        (8443, 443, "tcp"),     # HTTPS
        (5432, 5432, "tcp"),    # PostgreSQL
        (53, 53, "udp"),        # DNS
    ]
)
```

### Persistent Storage

```python
# Ephemeral (default) - data lost on box removal
boxlite.BoxOptions(image="postgres")

# Persistent - data survives stop/restart via QCOW2 disk
boxlite.BoxOptions(
    image="postgres",
    disk_size_gb=20,  # 20 GB persistent disk
)
```

## Examples Gallery

The `examples/python/` directory contains 9 comprehensive examples:

### 1. **simplebox_example.py** - Foundation Patterns
Demonstrates core BoxLite features:
- Basic command execution with results
- Separate stdout/stderr handling
- Environment variables and working directory
- Error handling and exit codes
- Multiple commands in same box
- Data processing pipeline

[View source](../../examples/python/simplebox_example.py)

### 2. **codebox_example.py** - AI Code Execution
Secure Python code execution for AI agents:
- Basic code execution
- Dynamic package installation
- Data processing (AI agent use case)
- Isolation demonstration

[View source](../../examples/python/codebox_example.py)

### 3. **browserbox_example.py** - Browser Automation
Browser automation with Puppeteer/Playwright:
- Basic Chromium setup
- Custom browser configurations (Firefox, WebKit)
- Cross-browser testing patterns
- Integration examples

[View source](../../examples/python/browserbox_example.py)

### 4. **computerbox_example.py** - Desktop Automation
Desktop interaction for agent workflows:
- 14 desktop functions (mouse, keyboard, screenshots)
- Screen size detection
- Workflow automation
- GUI interaction patterns

[View source](../../examples/python/computerbox_example.py)

### 5. **lifecycle_example.py** - Box Lifecycle Management
Managing box state:
- Stop and restart operations
- State persistence
- Data persistence verification
- Resource cleanup

[View source](../../examples/python/lifecycle_example.py)

### 6. **list_boxes_example.py** - Runtime Introspection
Enumerate and inspect boxes:
- List all boxes with status
- Display box metadata (ID, name, state, resources)
- Filter by status

[View source](../../examples/python/list_boxes_example.py)

### 7. **cross_process_example.py** - Multi-Process Operations
Cross-process box management:
- Reattach to running boxes from different processes
- Restart stopped boxes
- Multi-process runtime handling

[View source](../../examples/python/cross_process_example.py)

### 8. **interactivebox_example.py** - Interactive Shells
Direct shell access:
- Interactive terminal sessions
- Terminal mode handling
- Simple container experience

[View source](../../examples/python/interactivebox_example.py)

### 9. **native_example.py** - Low-Level API
Using the Rust API directly from Python:
- Default and custom runtime initialization
- Resource limits (CPU, memory, volumes, ports)
- Box information retrieval
- Streaming execution

[View source](../../examples/python/native_example.py)

## Metrics & Monitoring

### Runtime Metrics

Get aggregate metrics across all boxes:

```python
runtime = boxlite.Boxlite.default()
metrics = runtime.metrics()

print(f"Boxes created: {metrics.boxes_created}")
print(f"Boxes destroyed: {metrics.boxes_destroyed}")
print(f"Total exec calls: {metrics.total_exec_calls}")
```

**RuntimeMetrics Fields:**
- `boxes_created: int` - Total boxes created
- `boxes_destroyed: int` - Total boxes destroyed
- `total_exec_calls: int` - Total command executions
- `active_boxes: int` - Currently running boxes

### Box Metrics

Get per-box resource usage:

```python
box = runtime.create(boxlite.BoxOptions(image="alpine"))
metrics = await box.metrics()

print(f"CPU time: {metrics.cpu_time_ms}ms")
print(f"Memory: {metrics.memory_usage_bytes / (1024**2):.2f} MB")
print(f"Network sent: {metrics.network_bytes_sent}")
print(f"Network received: {metrics.network_bytes_received}")
```

**BoxMetrics Fields:**
- `cpu_time_ms: int` - Total CPU time in milliseconds
- `memory_usage_bytes: int` - Current memory usage
- `network_bytes_sent: int` - Total bytes sent
- `network_bytes_received: int` - Total bytes received

## Error Handling

### Exception Types

```python
from boxlite import BoxliteError, ExecError, TimeoutError, ParseError
```

**BoxliteError** - Base exception for all BoxLite errors

**ExecError** - Command execution failed

**TimeoutError** - Operation timed out

**ParseError** - Failed to parse output

### Common Error Patterns

```python
import boxlite

async def safe_execution():
    try:
        async with boxlite.SimpleBox(image="python:slim") as box:
            result = await box.exec("python", "script.py")

            # Check exit code
            if result.exit_code != 0:
                print(f"Command failed: {result.stderr}")

    except boxlite.BoxliteError as e:
        # Handle BoxLite-specific errors
        print(f"BoxLite error: {e}")
    except Exception as e:
        # Handle other errors
        print(f"Unexpected error: {e}")
```

## Troubleshooting

### Installation Issues

**Problem:** `pip install boxlite` fails

**Solutions:**
- Ensure Python 3.10+: `python --version`
- Update pip: `pip install --upgrade pip`
- Check platform support (macOS ARM64, Linux x86_64/ARM64 only)

### Runtime Errors

**Problem:** "KVM not available" error on Linux

**Solutions:**
```bash
# Check if KVM is loaded
lsmod | grep kvm

# Check if /dev/kvm exists
ls -l /dev/kvm

# Add user to kvm group (may require logout/login)
sudo usermod -aG kvm $USER
```

**Problem:** "Hypervisor.framework not available" on macOS

**Solutions:**
- Ensure macOS 12+ (Monterey or later)
- Verify Apple Silicon (ARM64) - Intel Macs not supported
- Check System Settings → Privacy & Security → Developer Tools

### Image Pull Failures

**Problem:** "Failed to pull image" error

**Solutions:**
- Check internet connectivity
- Verify image name and tag exist: `docker pull <image>`
- For private images, authenticate with registry first

### Performance Issues

**Problem:** Box is slow or unresponsive

**Solutions:**
```python
# Increase resource limits
boxlite.BoxOptions(
    cpus=4,          # More CPUs
    memory_mib=4096, # More memory
)

# Check metrics
metrics = await box.metrics()
print(f"Memory usage: {metrics.memory_usage_bytes / (1024**2):.2f} MB")
print(f"CPU time: {metrics.cpu_time_ms}ms")
```

### Debug Logging

Enable debug logging to troubleshoot issues:

```bash
# Set RUST_LOG environment variable
RUST_LOG=debug python script.py
```

Log levels: `trace`, `debug`, `info`, `warn`, `error`

## Contributing

We welcome contributions to the Python SDK!

### Development Setup

```bash
# Clone repository
git clone https://github.com/boxlite-labs/boxlite.git
cd boxlite

# Initialize submodules
git submodule update --init --recursive

# Build Python SDK in development mode
make dev:python
```

### Running Tests

```bash
# Install dev dependencies
pip install -e ".[dev]"

# Run tests
python -m pytest sdks/python/tests/
```

### Building Wheels

```bash
# Build portable wheel
make dist:python
```

## Further Documentation

- [BoxLite Main README](../../README.md) - Project overview
- [Architecture Documentation](../../docs/architecture/README.md) - How BoxLite works
- [Getting Started Guide](../../docs/getting-started/README.md) - Installation and setup
- [How-to Guides](../../docs/guides/README.md) - Practical guides
- [API Reference](../../docs/reference/README.md) - Complete API documentation

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](../../LICENSE) for details.
