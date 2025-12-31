# FAQ & Troubleshooting

Frequently asked questions and common issues with BoxLite.

## General Questions

### What is BoxLite?

BoxLite is an embeddable virtual machine runtime for secure, isolated code execution. Think of it as "SQLite for sandboxing" - a lightweight library you can embed directly in your application without requiring a daemon or root privileges.

### What's the difference between BoxLite and Docker?

| Feature | BoxLite | Docker |
|---------|---------|--------|
| **Isolation** | Hardware VM (KVM/Hypervisor.framework) | Container (namespaces/cgroups) |
| **Daemon** | No daemon required | Requires Docker daemon |
| **Root** | No root required | Typically needs root/sudo |
| **Architecture** | Embeddable library | Client-server architecture |
| **Use Case** | Embedded sandboxing, AI agents | Application deployment, CI/CD |
| **Startup** | ~1-2 seconds | ~100-500ms |
| **Isolation Level** | Separate kernel, hardware isolation | Shared kernel |

**When to use BoxLite:**
- AI agents that need full execution freedom
- Untrusted code execution
- Hardware-level isolation required
- Embedded in applications (no daemon)

**When to use Docker:**
- Application deployment
- Development environments
- CI/CD pipelines
- Established Docker workflows

### Do I need root or sudo?

**No.** BoxLite doesn't require root privileges.

**macOS:** Hypervisor.framework is available to all users (no special permissions)

**Linux:** Only requires access to `/dev/kvm`, which can be granted through group membership:

```bash
sudo usermod -aG kvm $USER
# Logout and login for changes to take effect
```

### Can I use BoxLite on Windows?

**No**, BoxLite is not supported on Windows.

**Alternatives:**
- Use WSL2 (Windows Subsystem for Linux) with Ubuntu
- Deploy to a Linux VM or cloud instance
- Consider cloud-based sandboxing solutions for Windows

**Why not Windows?**
BoxLite requires KVM (Linux) or Hypervisor.framework (macOS), neither of which are available on Windows.

### What Python versions are supported?

**Python 3.10 or later.**

Check your version:
```bash
python --version  # Should be 3.10+
```

Upgrade if needed:
```bash
# macOS (Homebrew)
brew install python@3.11

# Ubuntu/Debian
sudo apt install python3.11

# Or use pyenv
pyenv install 3.11.0
```

### Is BoxLite production-ready?

**Yes.** BoxLite v0.4.4 is stable and used in production.

**Production considerations:**
- ✅ Stable API (v0.4.x series)
- ✅ Hardware-level isolation
- ✅ Resource limits enforced
- ✅ Error handling robust
- ⚠️ Monitor resource usage
- ⚠️ Test at expected scale
- ⚠️ Configure appropriate limits

See [Deployment Patterns](./guides/README.md#deployment-patterns) for production checklist.

### What's the license?

Apache License 2.0. Free for commercial and non-commercial use.

See [LICENSE](../LICENSE) for details.

## Technical Questions

### What hypervisor does BoxLite use?

**macOS:** Hypervisor.framework (built into macOS 12+)

**Linux:** KVM (Kernel-based Virtual Machine)

**How it works:**
- BoxLite uses libkrun as the hypervisor abstraction
- libkrun provides a unified API over Hypervisor.framework (macOS) and KVM (Linux)
- Each box runs as a separate microVM with its own kernel

### How much memory does each box use?

**Minimum:** 128 MiB (configured via `memory_mib`)

**Default:** 512 MiB

**Range:** 128 MiB to 64 GiB (65536 MiB)

**Overhead:**
- VM overhead: ~50-100 MB per box
- Guest kernel: ~20-40 MB
- Container: Depends on image

**Example:**
```python
# Lightweight box
boxlite.BoxOptions(memory_mib=128)  # Minimum for Alpine

# Standard box
boxlite.BoxOptions(memory_mib=512)  # Default, good for Python

# Heavy box
boxlite.BoxOptions(memory_mib=2048)  # For complex workloads
```

### What's the box startup time?

**Typical:** 1-2 seconds

**Factors:**
- Image size (cached vs first pull)
- Disk I/O speed
- Available resources

**First run:** 5-30 seconds (includes image pull)

**Subsequent runs:** 1-2 seconds (image cached)

**Optimization:**
- Pre-pull images: `runtime.create(boxlite.BoxOptions(image="..."))`
- Reuse boxes instead of creating new ones
- Use smaller base images (`alpine:latest` vs `ubuntu:latest`)

### Can I persist data between boxes?

**Yes**, using persistent disks.

**Ephemeral (default):**
```python
boxlite.BoxOptions()  # Data lost when box is removed
```

**Persistent:**
```python
boxlite.BoxOptions(
    disk_size_gb=10  # 10 GB persistent QCOW2 disk
)

# Data survives stop/restart
await box.stop()
# ... later ...
box = runtime.get(box_id)  # Disk intact
```

**Also:**
- Use volume mounts for host-box data sharing
- Read-write volumes persist changes to host filesystem

### How do I debug BoxLite issues?

**1. Enable debug logging:**

```bash
RUST_LOG=debug python script.py
```

**2. Check box status:**

```python
info = await box.info()
print(f"Status: {info.status}")

metrics = await box.metrics()
print(f"Memory: {metrics.memory_usage_bytes / (1024**2):.2f} MB")
```

**3. Inspect filesystem:**

```bash
# Check disk space
df -h ~/.boxlite

# Check box data
ls -la ~/.boxlite/boxes/

# Check image cache
ls -la ~/.boxlite/images/
```

**4. Check hypervisor:**

```bash
# Linux
ls -l /dev/kvm
lsmod | grep kvm

# macOS
sw_vers  # Should be 12+
uname -m  # Should be arm64
```

See [Debugging Guide](./guides/README.md#debugging) for comprehensive troubleshooting.

## Networking

### Does BoxLite support internet access?

**Yes.** All boxes have full internet access by default.

**Outbound connections:**
- HTTP/HTTPS requests
- DNS resolution
- Any protocol (TCP/UDP)

**Example:**
```python
async with boxlite.SimpleBox(image="alpine:latest") as box:
    # Test internet access
    result = await box.exec("wget", "-O-", "https://api.github.com/zen")
    print(result.stdout)
```

### How do I expose ports from a box?

Use the `ports` parameter for port forwarding:

```python
boxlite.BoxOptions(
    ports=[
        (8080, 80, "tcp"),      # Host 8080 → Guest 80
        (5432, 5432, "tcp"),    # PostgreSQL
        (53, 53, "udp"),        # DNS (UDP)
    ]
)
```

**Access from host:**
```bash
curl http://localhost:8080
```

See [Configuring Networking](./guides/README.md#configuring-networking) for details.

### Can boxes communicate with each other?

**Not directly.** Boxes are isolated from each other.

**Alternatives:**
1. **Share data via volumes:**
   ```python
   volumes=[("/host/shared", "/mnt/shared", "rw")]
   ```

2. **Use host network:**
   - Box A exposes port
   - Box B connects to `host.docker.internal:port` (or localhost on Linux)

3. **External service:**
   - Both boxes connect to Redis/database on host or network

## Performance

### Why is my box slow?

**Common causes:**

1. **Insufficient resources:**
   ```python
   # Increase limits
   boxlite.BoxOptions(
       cpus=4,          # More CPUs
       memory_mib=4096, # More memory
   )
   ```

2. **Disk I/O:**
   - Use ephemeral storage (faster than QCOW2)
   - Check host disk speed: `dd if=/dev/zero of=test bs=1M count=1024`

3. **Too many boxes:**
   ```python
   metrics = runtime.metrics()
   print(f"Active boxes: {metrics.active_boxes}")
   # Reduce concurrency or increase host resources
   ```

4. **Image size:**
   - Use smaller images: `alpine:latest` (5 MB) vs `ubuntu:latest` (77 MB)
   - Check image size: `docker images`

### Can I run 100 boxes concurrently?

**It depends on host resources.**

**Resource calculation:**
```
Total Memory = (boxes * memory_mib) + overhead
Total CPUs = boxes * cpus (can oversubscribe)

Example:
100 boxes * 512 MiB = 51.2 GB memory needed
100 boxes * 1 CPU = 100 CPUs (oversubscribed, shares-based)
```

**Best practices:**
- Start small (10 boxes) and scale up
- Monitor metrics: `runtime.metrics().active_boxes`
- Use resource pooling (reuse boxes)
- Test at expected load

**Example:**
```python
import asyncio

async def run_100_boxes():
    tasks = []
    for i in range(100):
        task = run_box(i)
        tasks.append(task)

    results = await asyncio.gather(*tasks)
```

### What's the maximum box size?

**No hard limit**, but practical constraints:

**Memory:**
- Range: 128 MiB to 64 GiB (65536 MiB)
- Limited by host RAM

**Disk:**
- Range: 1 GB to 1 TB
- Limited by host storage

**CPUs:**
- Range: 1 to host CPU count
- Can oversubscribe (shares-based)

**Tested configurations:**
- ✅ 64 GiB memory
- ✅ 1 TB disk
- ✅ 16 CPUs

## Troubleshooting

### "Image pull failed" error

**Causes:**
1. Network connectivity issues
2. Invalid image name/tag
3. Private image requires authentication
4. Registry not reachable

**Solutions:**

```bash
# Test with Docker first
docker pull <image>

# Check network
ping registry-1.docker.io

# For private images, authenticate
docker login

# Check image name format
# Correct: "python:3.11-slim"
# Wrong: "python/3.11-slim"

# Clear cache if corrupted
rm -rf ~/.boxlite/images/*
```

**Debug:**
```bash
RUST_LOG=debug python script.py
# Look for image-related errors in output
```

### "Box fails to start" error

**Debug checklist:**

1. **Check disk space:**
   ```bash
   df -h ~/.boxlite
   # Should have at least 1 GB free
   ```

2. **Verify hypervisor:**
   ```bash
   # Linux
   ls -l /dev/kvm
   lsmod | grep kvm

   # macOS
   sw_vers | grep ProductVersion  # Should be 12+
   uname -m  # Should be arm64
   ```

3. **Check image:**
   ```bash
   docker pull <image>
   # Should succeed
   ```

4. **Enable debug logging:**
   ```bash
   RUST_LOG=debug python script.py
   ```

5. **Check permissions:**
   ```bash
   # Linux: Ensure user in kvm group
   groups | grep kvm

   # If not, add and relogin
   sudo usermod -aG kvm $USER
   ```

### "Command hangs" or "Execution timeout"

**Causes:**
1. Command is waiting for input
2. Long-running operation
3. Deadlock or infinite loop

**Solutions:**

```python
import asyncio

# Add timeout
async def execute_with_timeout():
    execution = await box.exec("command")

    try:
        result = await asyncio.wait_for(
            execution.wait(),
            timeout=30  # 30 second timeout
        )
        return result
    except asyncio.TimeoutError:
        await execution.kill()
        print("Command timed out")
```

**Check if command needs input:**
```python
# Provide stdin if needed
execution = await box.exec("command")
stdin = execution.stdin()
await stdin.write("input\n")
await stdin.close()
```

### "Port forward not working"

**Debug steps:**

1. **Check port is not in use:**
   ```bash
   lsof -i :8080
   # Should be empty, or show boxlite process
   ```

2. **Verify configuration:**
   ```python
   # Correct
   ports=[(8080, 80, "tcp")]

   # Wrong (swapped)
   # ports=[(80, 8080, "tcp")]  # Don't do this
   ```

3. **Test from inside box:**
   ```python
   # Start server in box
   await box.exec("python", "-m", "http.server", "80", background=True)

   # Test from host
   import requests
   response = requests.get("http://localhost:8080")
   ```

4. **Check gvproxy:**
   ```bash
   ps aux | grep gvproxy
   # Should show gvproxy process

   ls ~/.boxlite/gvproxy/
   # Should contain gvproxy binary
   ```

### "Permission denied" errors

**Common scenarios:**

**1. ~/.boxlite directory:**
```bash
chmod 755 ~/.boxlite
chown -R $USER ~/.boxlite
```

**2. /dev/kvm (Linux):**
```bash
# Check permissions
ls -l /dev/kvm
# Should be: crw-rw---- 1 root kvm

# Add user to kvm group
sudo usermod -aG kvm $USER
# Logout and login required

# Or temporarily (not recommended)
sudo chmod 666 /dev/kvm
```

**3. Volume mounts:**
```bash
# Ensure host path is accessible
chmod 755 /host/path
```

### "Out of memory" / "Box killed"

**Cause:** Box exceeded memory limit.

**Solutions:**

1. **Increase memory limit:**
   ```python
   boxlite.BoxOptions(
       memory_mib=2048,  # Increase from 512 to 2048
   )
   ```

2. **Check actual usage:**
   ```python
   metrics = await box.metrics()
   print(f"Memory: {metrics.memory_usage_bytes / (1024**2):.2f} MB")
   ```

3. **Optimize code:**
   - Reduce memory footprint of executed code
   - Process data in chunks instead of loading all at once
   - Clear variables when no longer needed

4. **Use swap (Linux only, not recommended):**
   - Better to increase `memory_mib`

### "KVM not available" (Linux)

**Cause:** KVM module not loaded or not accessible.

**Solutions:**

1. **Load KVM module:**
   ```bash
   sudo modprobe kvm kvm_intel  # For Intel CPUs
   sudo modprobe kvm kvm_amd    # For AMD CPUs

   # Verify
   lsmod | grep kvm
   ```

2. **Check CPU support:**
   ```bash
   grep -E 'vmx|svm' /proc/cpuinfo
   # Should show vmx (Intel) or svm (AMD)
   ```

3. **Enable in BIOS:**
   - Reboot and enter BIOS/UEFI
   - Enable "Intel VT-x" or "AMD-V"
   - Save and reboot

4. **Add user to kvm group:**
   ```bash
   sudo usermod -aG kvm $USER
   # Logout and login
   ```

### "Hypervisor.framework not available" (macOS)

**Cause:** Running on unsupported macOS version or architecture.

**Solutions:**

1. **Check macOS version:**
   ```bash
   sw_vers
   # ProductVersion should be 12.0 or higher
   ```

2. **Check architecture:**
   ```bash
   uname -m
   # Should output: arm64 (Apple Silicon)
   ```

3. **Upgrade if needed:**
   - BoxLite requires macOS 12+ (Monterey or later)
   - Apple Silicon (M1, M2, M3, M4) only
   - Intel Macs are **not supported**

**Note:** If you have an Intel Mac, consider:
- Using a Linux VM
- Deploying to cloud (AWS, GCP, Azure)
- Using a cloud-based sandboxing service

## Getting Help

### Where can I get help?

**Documentation:**
- [Getting Started](./getting-started/README.md) - Quick onboarding
- [Python SDK README](../sdks/python/README.md) - Complete Python API
- [How-to Guides](./guides/README.md) - Practical guides
- [Reference Documentation](./reference/README.md) - API and configuration reference
- [Architecture Documentation](./architecture/README.md) - How BoxLite works

**Community:**
- [GitHub Issues](https://github.com/boxlite-labs/boxlite/issues) - Bug reports and feature requests
- [GitHub Discussions](https://github.com/boxlite-labs/boxlite/discussions) - Questions and community support

**Before posting:**
1. Check this FAQ
2. Search existing issues/discussions
3. Enable debug logging: `RUST_LOG=debug`
4. Include BoxLite version, platform, and minimal reproduction

### How do I report a bug?

**1. Search existing issues:**
[GitHub Issues](https://github.com/boxlite-labs/boxlite/issues)

**2. Gather information:**
- BoxLite version: `python -c "import boxlite; print(boxlite.__version__)"`
- Platform: `uname -a`
- Python version: `python --version`
- Error message and stack trace

**3. Minimal reproduction:**
```python
import asyncio
import boxlite

async def reproduce():
    # Minimal code that reproduces the issue
    async with boxlite.SimpleBox(image="python:slim") as box:
        result = await box.exec("command")

asyncio.run(reproduce())
```

**4. Debug logs:**
```bash
RUST_LOG=debug python reproduce.py 2>&1 | tee debug.log
```

**5. Create issue:**
- Use bug report template
- Include all gathered information
- Attach debug logs if relevant
- Be specific and clear

### How do I request a feature?

**1. Check roadmap:**
- Review [GitHub Issues](https://github.com/boxlite-labs/boxlite/issues) with `enhancement` label

**2. Search for similar requests:**
- May already be planned or discussed

**3. Create feature request:**
- Use feature request template
- Describe use case (why you need it)
- Provide examples of desired API/behavior
- Explain benefits to other users

**4. Participate in discussion:**
- Respond to questions
- Refine proposal based on feedback
- Consider implementing it yourself (see [CONTRIBUTING.md](../CONTRIBUTING.md))

### How do I contribute?

See [CONTRIBUTING.md](../CONTRIBUTING.md) for:
- Development setup
- Running tests
- Code style guidelines
- Pull request process

**Quick start:**
```bash
git clone https://github.com/boxlite-labs/boxlite.git
cd boxlite
git submodule update --init --recursive
make setup
make dev:python
```

**Areas to contribute:**
- Bug fixes
- Documentation improvements
- New examples
- SDK improvements (Python, Node.js, C)
- Performance optimizations
