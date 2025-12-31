# Getting Started

Get up and running with BoxLite in 5 minutes.

## Prerequisites

### System Requirements

BoxLite requires a platform with hardware virtualization support:

| Platform | Architecture  | Requirements                        |
|----------|---------------|-------------------------------------|
| macOS    | Apple Silicon | macOS 12+ (Monterey or later)       |
| Linux    | x86_64        | KVM enabled (`/dev/kvm` accessible) |
| Linux    | ARM64         | KVM enabled (`/dev/kvm` accessible) |

**Not Supported:**
- macOS Intel (x86_64) - Hypervisor.framework stability issues
- Windows - Use WSL2 with Linux requirements

### Verify Virtualization Support

**macOS:**
```bash
# Check macOS version (should be 12+)
sw_vers

# Check architecture (should be arm64)
uname -m
```

**Linux:**
```bash
# Check if CPU supports virtualization (should show vmx or svm)
grep -E 'vmx|svm' /proc/cpuinfo

# Check if KVM is available (should exist and be accessible)
ls -l /dev/kvm

# If /dev/kvm doesn't exist, load KVM module
sudo modprobe kvm
sudo modprobe kvm_intel  # For Intel CPUs
sudo modprobe kvm_amd    # For AMD CPUs

# Add user to kvm group (may require logout/login)
sudo usermod -aG kvm $USER
```

### No Daemon Required

Unlike Docker, BoxLite doesn't require a daemon process. It's an embeddable library that runs directly in your application.

## Installation

### Python SDK (Recommended)

The easiest way to get started with BoxLite is through the Python SDK:

```bash
pip install boxlite
```

**Requirements:**
- Python 3.10 or later
- pip 20.0+

**Verify Installation:**
```python
python3 -c "import boxlite; print(boxlite.__version__)"
# Output: 0.4.4
```

### Rust

Add BoxLite to your `Cargo.toml`:

```toml
[dependencies]
boxlite = { git = "https://github.com/boxlite-labs/boxlite" }
tokio = { version = "1", features = ["full"] }
futures = "0.3"
```

### From Source (Development)

For contributing or local development:

```bash
# Clone repository
git clone https://github.com/boxlite-labs/boxlite.git
cd boxlite

# Initialize submodules (critical!)
git submodule update --init --recursive

# Install platform dependencies
make setup

# Build Python SDK in development mode
make dev:python

# Verify build
python3 -c "import boxlite; print('Success!')"
```

See [CONTRIBUTING.md](../../CONTRIBUTING.md) for detailed build instructions.

### Node.js

Coming soon.

### Go

Coming soon.

## Quick Start

### Python - Basic Execution

Create a file `hello.py`:

```python
import asyncio
import boxlite


async def main():
    # Create a box and run a command
    async with boxlite.SimpleBox(image="python:slim") as box:
        result = await box.exec("python", "-c", "print('Hello from BoxLite!')")
        print(result.stdout)
        # Output: Hello from BoxLite!


if __name__ == "__main__":
    asyncio.run(main())
```

Run it:
```bash
python hello.py
```

**What's happening:**
1. BoxLite pulls the `python:slim` OCI image (first run only)
2. Creates a lightweight VM with the image
3. Executes the Python command inside the VM
4. Streams output back to your application
5. Automatically cleans up when the context exits

### Python - Code Execution (AI Agents)

Create a file `codebox.py`:

```python
import asyncio
import boxlite


async def main():
    # Execute untrusted code safely
    code = """
import requests
response = requests.get('https://api.github.com/zen')
print(response.text)
"""

    async with boxlite.CodeBox() as codebox:
        result = await codebox.run(code)
        print(result)


if __name__ == "__main__":
    asyncio.run(main())
```

Run it:
```bash
python codebox.py
```

**What's happening:**
1. CodeBox automatically installs required packages (requests)
2. Executes the code in complete isolation
3. Returns the output
4. Your host system remains completely safe

### Rust - Basic Execution

Create a file `src/main.rs`:

```rust
use boxlite::{BoxliteRuntime, BoxOptions, BoxCommand, RootfsSpec};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create runtime
    let runtime = BoxliteRuntime::default_runtime();

    // Create box
    let options = BoxOptions {
        rootfs: RootfsSpec::Image("alpine:latest".into()),
        ..Default::default()
    };
    let (_, litebox) = runtime.create(options)?;

    // Execute command
    let mut execution = litebox
        .exec(BoxCommand::new("echo").arg("Hello from BoxLite!"))
        .await?;

    // Stream stdout
    let mut stdout = execution.stdout().unwrap();
    while let Some(line) = stdout.next().await {
        println!("{}", line);
    }

    Ok(())
}
```

Run it:
```bash
cargo run
```

### Running Examples

BoxLite includes 9 comprehensive Python examples covering all major use cases:

```bash
# Clone the repository
git clone https://github.com/boxlite-labs/boxlite.git
cd boxlite

# Build Python SDK
make dev:python

# Run examples
python examples/python/simplebox_example.py
python examples/python/codebox_example.py
python examples/python/browserbox_example.py
python examples/python/computerbox_example.py
python examples/python/lifecycle_example.py
python examples/python/list_boxes_example.py
python examples/python/cross_process_example.py
python examples/python/interactivebox_example.py
python examples/python/native_example.py
```

See [How-to Guides: Running Examples](../guides/README.md#running-examples) for detailed walkthrough of each example.

## Next Steps

### Learn the Python SDK

Read the comprehensive Python SDK documentation:

- **[Python SDK README](../../sdks/python/README.md)** - Complete API reference, examples, and patterns
- Focus on:
  - Core API Reference (Boxlite, BoxOptions, Box, Execution)
  - Higher-level APIs (SimpleBox, CodeBox, BrowserBox, ComputerBox)
  - API Patterns (async/await, context managers, streaming I/O)
  - Configuration Reference (resources, volumes, ports)

### Explore Examples

Study the 9 Python examples in `examples/python/`:

1. **simplebox_example.py** - Foundation patterns
2. **codebox_example.py** - AI code execution
3. **browserbox_example.py** - Browser automation
4. **computerbox_example.py** - Desktop automation
5. **lifecycle_example.py** - Box lifecycle management
6. **list_boxes_example.py** - Runtime introspection
7. **cross_process_example.py** - Multi-process operations
8. **interactivebox_example.py** - Interactive shells
9. **native_example.py** - Low-level Rust API

Each example is well-documented with comments explaining the patterns.

### Configure Resources

Learn how to control box resources:

```python
import boxlite

options = boxlite.BoxOptions(
    image="postgres:latest",
    cpus=2,                    # 2 CPU cores
    memory_mib=1024,          # 1 GB RAM
    disk_size_gb=10,          # 10 GB persistent disk
    env=[
        ("POSTGRES_PASSWORD", "secret"),
    ],
    volumes=[
        ("/host/data", "/mnt/data", "ro"),  # Read-only mount
    ],
    ports=[
        (5432, 5432, "tcp"),  # Port forwarding
    ],
)

runtime = boxlite.Boxlite.default()
box = runtime.create(options)
```

See [Reference: Configuration](../reference/README.md#configuration-reference) for all options.

### Read Architecture Documentation

Understand how BoxLite works under the hood:

- **[Architecture Documentation](../architecture/README.md)** - Comprehensive system design
  - Core components (Runtime, LiteBox, VMM, Portal)
  - Image management and rootfs preparation
  - Network backends (gvproxy, libslirp)
  - Host-guest communication protocol

### Practical Guides

Learn practical patterns:

- **[How-to Guides](../guides/README.md)** - Practical usage guides
  - Building from source
  - Running examples
  - Configuring networking
  - Volume mounting
  - Debugging
  - Resource limits & tuning
  - Using with AI agents
  - Integration examples
  - Deployment patterns

### Deploy to Production

When ready for production:

- Review [How-to Guides: Deployment Patterns](../guides/README.md#deployment-patterns)
- Configure appropriate resource limits
- Set up monitoring with metrics
- Implement error handling
- Test at scale

### Get Help

If you run into issues:

- **[FAQ & Troubleshooting](../faq.md)** - Common questions and solutions
- **[GitHub Issues](https://github.com/boxlite-labs/boxlite/issues)** - Bug reports and feature requests
- **[GitHub Discussions](https://github.com/boxlite-labs/boxlite/discussions)** - Questions and community support

### Contribute

Want to contribute?

- **[CONTRIBUTING.md](../../CONTRIBUTING.md)** - Contribution guidelines
  - Development setup
  - Running tests
  - Code style
  - Pull request process

## Troubleshooting Quick Links

### Installation Issues

**Problem:** `pip install boxlite` fails

**Solutions:**
- Verify Python 3.10+: `python --version`
- Update pip: `pip install --upgrade pip`
- Check platform support (macOS ARM64, Linux x86_64/ARM64 only)

### Runtime Issues

**Problem:** "KVM not available" error on Linux

**Solutions:**
```bash
# Check KVM module
lsmod | grep kvm

# Load KVM module
sudo modprobe kvm kvm_intel  # or kvm_amd

# Check /dev/kvm permissions
ls -l /dev/kvm
sudo chmod 666 /dev/kvm  # or add user to kvm group
```

**Problem:** Box fails to start

**Solutions:**
- Check disk space: `df -h ~/.boxlite`
- Enable debug logging: `RUST_LOG=debug python script.py`
- Verify image name: Try `docker pull <image>` to test
- Check hypervisor: Ensure KVM (Linux) or Hypervisor.framework (macOS) is available

### Performance Issues

**Problem:** Box is slow

**Solutions:**
```python
# Increase resource limits
boxlite.BoxOptions(
    cpus=4,          # More CPUs
    memory_mib=4096, # More memory
)

# Check metrics
metrics = await box.metrics()
print(f"Memory: {metrics.memory_usage_bytes / (1024**2):.2f} MB")
```

For more troubleshooting help, see [FAQ & Troubleshooting](../faq.md).
