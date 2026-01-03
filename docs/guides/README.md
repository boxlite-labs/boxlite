# How-to Guides

## Building from Source

### Prerequisites

- Rust 1.75+ (stable)
- macOS (Apple Silicon) or Linux (x86_64/ARM64) with KVM
- Python 3.10+ (for Python SDK development)

### Quick Start

```bash
# Clone the repository
git clone https://github.com/boxlite-labs/boxlite.git
cd boxlite

# Initialize submodules
git submodule update --init --recursive

# Build
make setup
make dev:python
```

### Makefile Targets

| Target             | Description                              |
|--------------------|------------------------------------------|
| `make setup`       | Install platform-specific dependencies   |
| `make guest`       | Cross-compile guest binary (musl static) |
| `make shim`        | Build boxlite-shim binary                |
| `make runtime`     | Build complete BoxLite runtime           |
| `make dev:python`  | Local Python SDK development             |
| `make dist:python` | Build portable Python wheels             |
| `make clean`       | Clean build artifacts                    |

### Platform Support

| Platform | Architecture          | Hypervisor           |
|----------|-----------------------|----------------------|
| macOS    | ARM64 (Apple Silicon) | Hypervisor.framework |
| Linux    | x86_64                | KVM                  |
| Linux    | ARM64                 | KVM                  |

### Build Scripts

Build scripts are located in `scripts/`:

```
scripts/
├── setup/              # Platform-specific setup
│   ├── macos.sh
│   ├── ubuntu.sh
│   ├── manylinux.sh
│   └── musllinux.sh
├── build/              # Build scripts
│   ├── guest.sh        # Guest binary (cross-compile)
│   ├── shim.sh         # Shim binary
│   └── build-runtime.sh
├── package/            # Packaging scripts
└── common.sh           # Shared utilities
```

## Running Examples

BoxLite includes 9 comprehensive Python examples demonstrating all major use cases.

### Prerequisites

```bash
# Clone repository
git clone https://github.com/boxlite-labs/boxlite.git
cd boxlite

# Build Python SDK
make dev:python
```

### Example Gallery

#### 1. SimpleBox - Foundation Patterns

**File:** `examples/python/simplebox_example.py`

Demonstrates core BoxLite features:
- Basic command execution
- Stdout/stderr separation
- Environment variables
- Working directory
- Error handling
- Multiple commands in same box

**Run:**
```bash
python examples/python/simplebox_example.py
```

**Key Patterns:**
```python
async with boxlite.SimpleBox(image="python:alpine") as box:
    # Execute command
    result = await box.exec("ls", "-lh", "/")
    print(result.stdout)

    # With environment variables
    result = await box.exec(
        "python", "-c", "import os; print(os.getenv('MY_VAR'))",
        env=[("MY_VAR", "value")]
    )
```

#### 2. CodeBox - AI Code Execution

**File:** `examples/python/codebox_example.py`

Secure Python code execution for AI agents.

**Run:**
```bash
python examples/python/codebox_example.py
```

**Key Patterns:**
```python
async with boxlite.CodeBox() as codebox:
    # Install packages automatically
    await codebox.install_package("requests")

    # Run untrusted code safely
    result = await codebox.run("""
import requests
response = requests.get('https://api.github.com/zen')
print(response.text)
""")
```

#### 3. BrowserBox - Browser Automation

**File:** `examples/python/browserbox_example.py`

**Run:**
```bash
python examples/python/browserbox_example.py
```

**Use Cases:**
- Web scraping
- E2E testing
- Browser automation
- Screenshot generation

#### 4. ComputerBox - Desktop Automation

**File:** `examples/python/computerbox_example.py`

**Run:**
```bash
python examples/python/computerbox_example.py
```

**Available Functions:**
- `screenshot()` - Capture screen
- `left_click()`, `right_click()`, `double_click()`
- `type_text(text)` - Type text
- `get_screen_size()` - Get dimensions
- `move_mouse(x, y)` - Move cursor
- And 9 more functions

#### 5. Lifecycle Management

**File:** `examples/python/lifecycle_example.py`

Demonstrates box state management.

**Run:**
```bash
python examples/python/lifecycle_example.py
```

#### 6-9. Other Examples

- `list_boxes_example.py` - Runtime introspection
- `cross_process_example.py` - Multi-process operations
- `interactivebox_example.py` - Interactive shells
- `native_example.py` - Low-level Rust API

### Customizing Examples

All examples can be customized by editing the source files:

**Change Image:**
```python
async with boxlite.SimpleBox(image="ubuntu:22.04") as box:
    # ...
```

**Add Resources:**
```python
async with boxlite.SimpleBox(
    image="python:slim",
    cpus=2,
    memory_mib=2048
) as box:
    # ...
```

**Mount Volumes:**
```python
async with boxlite.SimpleBox(
    image="python:slim",
    volumes=[("/host/data", "/mnt/data", "ro")]
) as box:
    # ...
```

## Configuring Networking

BoxLite provides full internet access and port forwarding through gvproxy.

### Network Modes

BoxLite uses gvproxy for NAT networking by default. All boxes can:
- Access the internet
- Resolve DNS
- Make outbound connections

### Port Forwarding

Map host ports to guest ports for incoming connections.

**Basic Port Forwarding:**

```python
import boxlite

options = boxlite.BoxOptions(
    image="python:slim",
    ports=[
        (8080, 80, "tcp"),      # Host 8080 → Guest 80 (HTTP)
        (8443, 443, "tcp"),     # Host 8443 → Guest 443 (HTTPS)
    ]
)

runtime = boxlite.Boxlite.default()
box = runtime.create(options)
```

**Multiple Ports:**

```python
ports=[
    (8080, 80, "tcp"),      # HTTP
    (8443, 443, "tcp"),     # HTTPS
    (5432, 5432, "tcp"),    # PostgreSQL
    (6379, 6379, "tcp"),    # Redis
    (53, 53, "udp"),        # DNS (UDP)
]
```

**Custom Port Mapping:**

```python
# Map host port 3000 to guest port 8000
ports=[(3000, 8000, "tcp")]
```

### Testing Connectivity

**From Host to Box:**

```python
import asyncio
import boxlite
import requests

async def test_connectivity():
    async with boxlite.SimpleBox(
        image="python:slim",
        ports=[(8080, 8000, "tcp")]
    ) as box:
        # Start web server in box
        await box.exec("python", "-m", "http.server", "8000", background=True)

        # Test from host
        response = requests.get("http://localhost:8080")
        print(f"Status: {response.status_code}")

asyncio.run(test_connectivity())
```

**From Box to Internet:**

```python
async with boxlite.SimpleBox(image="alpine:latest") as box:
    # Test DNS
    result = await box.exec("nslookup", "google.com")
    print(result.stdout)

    # Test HTTP
    result = await box.exec("wget", "-O-", "https://api.github.com/zen")
    print(result.stdout)
```

### Network Metrics

Monitor network usage:

```python
box = runtime.create(boxlite.BoxOptions(image="alpine"))
metrics = await box.metrics()

print(f"Bytes sent: {metrics.network_bytes_sent}")
print(f"Bytes received: {metrics.network_bytes_received}")
```

### Troubleshooting Networking

**Problem:** Port forward not working

**Solutions:**
```bash
# Check if port is in use
lsof -i :8080

# Stop conflicting process or use different port
```

**Problem:** Cannot access internet from box

**Solutions:**
```bash
# Verify gvproxy is running
ps aux | grep gvproxy

# Check DNS resolution
# (run inside box)
nslookup google.com
```

## Volume Mounting

Mount host directories into boxes for data input/output.

### Mount Types

**virtiofs (Default):**
- High-performance file sharing
- Low overhead
- Real-time host-guest synchronization

**QCOW2 (Persistent Disk):**
- Block device
- Survives box restarts
- Copy-on-write

### Read-Only vs Read-Write

**Read-Only Mount (Data Input):**

```python
volumes=[
    ("/host/config", "/etc/app/config", "ro"),
    ("/host/datasets", "/mnt/data", "ro"),
]
```

**Read-Write Mount (Data Output):**

```python
volumes=[
    ("/host/output", "/mnt/output", "rw"),
    ("/host/logs", "/var/log/app", "rw"),
]
```

### Common Use Cases

#### 1. Configuration Files

```python
import os
import boxlite

# Mount config directory
async with boxlite.SimpleBox(
    image="python:slim",
    volumes=[
        (os.path.expanduser("~/.config/myapp"), "/etc/myapp", "ro")
    ]
) as box:
    result = await box.exec("cat", "/etc/myapp/config.yaml")
    print(result.stdout)
```

#### 2. Data Processing

```python
# Input data (read-only), output results (read-write)
async with boxlite.SimpleBox(
    image="python:slim",
    volumes=[
        ("/data/input", "/mnt/input", "ro"),
        ("/data/output", "/mnt/output", "rw"),
    ]
) as box:
    await box.exec("python", "process.py", "--input", "/mnt/input", "--output", "/mnt/output")
```

#### 3. Source Code Development

```python
# Mount source code for live development
async with boxlite.SimpleBox(
    image="python:slim",
    volumes=[
        (os.getcwd(), "/workspace", "rw")
    ],
    working_dir="/workspace"
) as box:
    # Run tests in isolated environment
    await box.exec("pytest", "tests/")
```

#### 4. Persistent Storage with QCOW2

```python
# Create box with persistent disk
box = runtime.create(boxlite.BoxOptions(
    image="postgres:latest",
    disk_size_gb=20,  # 20 GB persistent disk
    env=[("POSTGRES_PASSWORD", "secret")],
))

# Data survives stop/restart
await box.stop()
# ... later ...
box = runtime.get(box.id)  # Disk still intact
```

### Performance Considerations

**virtiofs Performance:**
- Fast for small files
- Slight overhead for large files
- Real-time synchronization

**QCOW2 Performance:**
- Block-level access (faster for large files)
- Copy-on-write overhead
- No real-time sync with host

**Best Practices:**
- Use read-only mounts when possible (lower overhead)
- Mount specific directories, not entire filesystem
- For large datasets, consider QCOW2 disk

## Debugging

Enable debug logging and inspect box state for troubleshooting.

### Enable Debug Logging

**Python:**

```bash
# Debug logging
RUST_LOG=debug python script.py

# Trace logging (very verbose)
RUST_LOG=trace python script.py

# Module-specific logging
RUST_LOG=boxlite::runtime=debug python script.py
```

**Rust:**

```bash
RUST_LOG=debug cargo run
```

**Log Levels:**
- `trace` - Very verbose, all details
- `debug` - Debug information
- `info` - Informational messages
- `warn` - Warnings
- `error` - Errors only

### Inspect Box State

**Get Box Information:**

```python
box = runtime.create(boxlite.BoxOptions(image="alpine"))
info = await box.info()

print(f"ID: {info.id}")
print(f"Status: {info.status}")
print(f"Image: {info.image}")
print(f"CPUs: {info.cpus}")
print(f"Memory: {info.memory_mib} MiB")
print(f"Created: {info.created_at}")
```

**Get Box Metrics:**

```python
metrics = await box.metrics()

print(f"CPU time: {metrics.cpu_time_ms}ms")
print(f"Memory usage: {metrics.memory_usage_bytes / (1024**2):.2f} MB")
print(f"Network sent: {metrics.network_bytes_sent}")
print(f"Network received: {metrics.network_bytes_received}")
```

**List All Boxes:**

```python
boxes = runtime.list()
for info in boxes:
    print(f"{info.id}: {info.status} ({info.image})")
```

### Common Issues & Debug Steps

#### Issue: Box Fails to Start

**Debug Steps:**

1. Check disk space:
   ```bash
   df -h ~/.boxlite
   ```

2. Enable debug logging:
   ```bash
   RUST_LOG=debug python script.py
   ```

3. Verify image exists:
   ```bash
   docker pull <image>
   ```

4. Check hypervisor:
   ```bash
   # Linux
   ls -l /dev/kvm
   grep -E 'vmx|svm' /proc/cpuinfo

   # macOS
   sw_vers  # Should be 12+
   uname -m  # Should be arm64
   ```

#### Issue: Command Execution Fails

**Debug Steps:**

1. Check exit code:
   ```python
   result = await box.exec("command")
   if result.exit_code != 0:
       print(f"Exit code: {result.exit_code}")
       print(f"Stderr: {result.stderr}")
   ```

2. Verify command exists:
   ```python
   result = await box.exec("which", "python3")
   print(result.stdout)  # Should print path
   ```

3. Check working directory:
   ```python
   result = await box.exec("pwd")
   print(result.stdout)
   ```

#### Issue: Performance Problems

**Debug Steps:**

1. Check resource usage:
   ```python
   metrics = await box.metrics()
   print(f"Memory: {metrics.memory_usage_bytes / (1024**2):.2f} MB")
   print(f"CPU time: {metrics.cpu_time_ms}ms")
   ```

2. Increase limits:
   ```python
   boxlite.BoxOptions(
       cpus=4,
       memory_mib=4096,
   )
   ```

3. Monitor runtime metrics:
   ```python
   runtime_metrics = runtime.metrics()
   print(f"Active boxes: {runtime_metrics.active_boxes}")
   print(f"Total exec calls: {runtime_metrics.total_exec_calls}")
   ```

### Log Locations

**Runtime Logs:**
- Location: `~/.boxlite/logs/`
- Enable with: `RUST_LOG=debug`

**Guest Logs:**
- Inside box: `/var/log/`
- Requires persistent disk to access after box stops

**Database:**
- `~/.boxlite/db/boxes.db`
- `~/.boxlite/db/images.db`

**Inspect Database:**

```bash
sqlite3 ~/.boxlite/db/boxes.db
.tables
SELECT * FROM boxes;
```

## Resource Limits & Tuning

Configure and optimize box resource usage.

### CPU Configuration

**Set CPU Count:**

```python
boxlite.BoxOptions(
    cpus=2,  # 2 CPU cores
)
```

**Range:** 1 to host CPU count

**Behavior:**
- Proportional scheduling (shares-based)
- Does not reserve physical cores
- Multiple boxes can exceed host CPU count

**Monitor Usage:**

```python
metrics = await box.metrics()
print(f"CPU time: {metrics.cpu_time_ms}ms")
```

### Memory Management

**Set Memory Limit:**

```python
boxlite.BoxOptions(
    memory_mib=1024,  # 1 GB
)
```

**Range:** 128 to 65536 MiB (64 GiB)

**Default:** 512 MiB

**Behavior:**
- Hard limit (box killed if exceeded)
- Minimum 128 MiB required

**Monitor Usage:**

```python
metrics = await box.metrics()
memory_mb = metrics.memory_usage_bytes / (1024**2)
print(f"Memory: {memory_mb:.2f} MB")
```

**Out of Memory:**
- Box process is killed
- Check stderr for OOM messages
- Increase `memory_mib` if needed

### Disk Configuration

**Ephemeral (Default):**

```python
boxlite.BoxOptions(
    disk_size_gb=None  # No persistent disk
)
```

**Persistent:**

```python
boxlite.BoxOptions(
    disk_size_gb=20  # 20 GB persistent disk
)
```

**Performance:**
- Ephemeral: Fastest (in-memory/tmpfs)
- QCOW2: Moderate (copy-on-write overhead)

**I/O Monitoring:**
- Currently not exposed in metrics
- Future feature

### Scaling Multiple Boxes

**Resource Pooling:**

```python
import asyncio
import boxlite

async def run_box(box_id):
    async with boxlite.SimpleBox(
        image="python:slim",
        cpus=1,
        memory_mib=512,
    ) as box:
        result = await box.exec("python", "-c", f"print('Box {box_id}')")
        return result.stdout

async def main():
    # Run 10 boxes concurrently
    tasks = [run_box(i) for i in range(10)]
    results = await asyncio.gather(*tasks)

    for i, result in enumerate(results):
        print(f"Box {i}: {result}")

asyncio.run(main())
```

**Concurrency Limits:**
- Limited by host resources (CPU, memory)
- Each box: minimum 128 MiB + overhead
- Monitor with `runtime.metrics().active_boxes`

**Best Practices:**
- Use asyncio for concurrent execution
- Configure appropriate resource limits
- Monitor metrics to avoid oversubscription

## Using with AI Agents

BoxLite is designed for AI agents that need full execution freedom.

### CodeBox for AI Code Execution

**Use Case:** AI generates Python code that needs execution.

**Example:**

```python
import asyncio
import boxlite

async def execute_ai_code(code: str):
    """Execute untrusted AI-generated code safely."""
    async with boxlite.CodeBox() as codebox:
        try:
            result = await codebox.run(code)
            return {"success": True, "output": result}
        except Exception as e:
            return {"success": False, "error": str(e)}

# AI-generated code
ai_code = """
import requests
response = requests.get('https://api.github.com/repos/python/cpython')
data = response.json()
print(f"Stars: {data['stargazers_count']}")
"""

result = asyncio.run(execute_ai_code(ai_code))
print(result)
```

### Multiple Tools in One Box

AI agents often need multiple tools. BoxLite provides a full Linux environment.

**Example:**

```python
async with boxlite.SimpleBox(image="python:slim") as box:
    # File system access
    await box.exec("mkdir", "-p", "/workspace")

    # Python code execution
    await box.exec("python", "-c", "print('Hello')")

    # Package installation
    await box.exec("pip", "install", "requests")

    # Network requests
    await box.exec("curl", "https://api.github.com/zen")

    # File manipulation
    await box.exec("echo", "data", ">", "/workspace/file.txt")
```

### Capturing Output

**Streaming Output:**

```python
execution = await box.exec("python", "long_running_script.py")

# Stream stdout in real-time
stdout = execution.stdout()
async for line in stdout:
    print(f"AI Output: {line}")

    # Parse and react to output
    if "ERROR" in line:
        await execution.kill()  # Stop on error
        break
```

**Exit Codes:**

```python
result = await box.exec("command")

if result.exit_code == 0:
    print("Success!")
else:
    print(f"Failed with code {result.exit_code}")
    print(f"Error: {result.stderr}")
```

### Security Considerations

**Isolation:**
- Hardware-level VM isolation (not just containers)
- AI cannot escape to host system
- Network access can be controlled

**Resource Limits:**
```python
# Prevent AI from consuming all resources
boxlite.BoxOptions(
    cpus=2,               # Limit CPUs
    memory_mib=1024,      # Limit memory
    disk_size_gb=10,      # Limit disk
    # No port forwarding = no incoming connections
)
```

**Timeout Handling:**

```python
import asyncio

async def execute_with_timeout(box, command, timeout=30):
    """Execute with timeout to prevent infinite loops."""
    try:
        execution = await box.exec(*command)
        result = await asyncio.wait_for(
            execution.wait(),
            timeout=timeout
        )
        return result
    except asyncio.TimeoutError:
        await execution.kill()
        raise TimeoutError(f"Command exceeded {timeout}s timeout")
```

### Performance Tips

**Reuse Boxes:**

```python
# Create once, use many times
box = runtime.create(boxlite.BoxOptions(image="python:slim"))

for code in ai_generated_codes:
    result = await box.exec("python", "-c", code)
    # Process result

# Cleanup when done
await box.remove()
```

**Batch Operations:**

```python
# Execute multiple commands in one box (faster than creating new boxes)
async with boxlite.SimpleBox(image="python:slim") as box:
    await box.exec("pip", "install", "requests")
    result1 = await box.exec("python", "script1.py")
    result2 = await box.exec("python", "script2.py")
    result3 = await box.exec("python", "script3.py")
```

**Monitor Resources:**

```python
metrics = await box.metrics()
if metrics.memory_usage_bytes > 0.8 * (1024**3):  # 80% of 1GB
    print("Warning: High memory usage")
    # Consider recreating box or increasing limit
```

## Integration Examples

### FastAPI Integration

Expose BoxLite as a REST API:

```python
from fastapi import FastAPI, HTTPException
from pydantic import BaseModel
import boxlite
import asyncio

app = FastAPI()

class CodeRequest(BaseModel):
    code: str
    timeout: int = 30

@app.post("/execute")
async def execute_code(request: CodeRequest):
    """Execute Python code in isolated box."""
    try:
        async with boxlite.CodeBox() as codebox:
            result = await asyncio.wait_for(
                codebox.run(request.code),
                timeout=request.timeout
            )
            return {"output": result}
    except asyncio.TimeoutError:
        raise HTTPException(status_code=408, detail="Execution timeout")
    except Exception as e:
        raise HTTPException(status_code=500, detail=str(e))

# Run: uvicorn main:app --reload
```

### Celery Task Queue

Background task processing with BoxLite:

```python
from celery import Celery
import boxlite
import asyncio

app = Celery('tasks', broker='redis://localhost:6379')

@app.task
def run_code_task(code: str):
    """Run code in box as background task."""
    async def execute():
        async with boxlite.CodeBox() as codebox:
            return await codebox.run(code)

    return asyncio.run(execute())

# Usage: run_code_task.delay("print('Hello')")
```

### Serverless Function Handler

AWS Lambda / Cloud Functions integration:

```python
import boxlite
import asyncio

def handler(event, context):
    """Serverless function handler."""
    code = event.get('code', '')

    async def execute():
        async with boxlite.SimpleBox(image="python:slim") as box:
            result = await box.exec("python", "-c", code)
            return {
                'statusCode': 200,
                'body': result.stdout
            }

    return asyncio.run(execute())
```

## Deployment Patterns

### Production Checklist

Before deploying BoxLite to production:

- [ ] **Resource Limits Configured**
  - Set appropriate `cpus` and `memory_mib`
  - Configure `disk_size_gb` if persistence needed
  - Test resource consumption under load

- [ ] **Error Handling Robust**
  - Catch all exceptions
  - Log errors appropriately
  - Implement retry logic if needed
  - Handle timeout scenarios

- [ ] **Logging/Monitoring Enabled**
  - Configure `RUST_LOG` for production logging
  - Monitor box metrics
  - Track runtime metrics
  - Set up alerting for failures

- [ ] **Performance Tested**
  - Load test with expected concurrency
  - Measure box startup time
  - Test resource limits under stress
  - Verify cleanup happens correctly

- [ ] **Security Review**
  - Verify network isolation configured correctly
  - Check resource limits prevent DoS
  - Review error messages (no sensitive data leaked)
  - Audit code execution paths

### Docker Container Deployment

Run BoxLite inside Docker (requires privileged mode for KVM):

```dockerfile
FROM ubuntu:22.04

# Install dependencies
RUN apt-get update && apt-get install -y \
    python3 \
    python3-pip \
    && rm -rf /var/lib/apt/lists/*

# Install BoxLite
RUN pip3 install boxlite

# Copy application
COPY app.py /app/app.py

WORKDIR /app

CMD ["python3", "app.py"]
```

**Run with KVM access:**

```bash
docker run --privileged --device /dev/kvm:/dev/kvm myapp
```

**Notes:**
- Requires `--privileged` and `--device /dev/kvm`
- Not recommended for multi-tenant environments (security)
- Consider VM-based deployment instead

### Kubernetes Deployment

Deploy BoxLite on Kubernetes:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: boxlite-app
spec:
  containers:
  - name: app
    image: myapp:latest
    securityContext:
      privileged: true  # Required for KVM
    volumeMounts:
    - name: dev-kvm
      mountPath: /dev/kvm
    resources:
      limits:
        memory: "4Gi"
        cpu: "2"
  volumes:
  - name: dev-kvm
    hostPath:
      path: /dev/kvm
      type: CharDevice
```

**Notes:**
- Requires privileged containers (security consideration)
- Only works on KVM-enabled nodes
- Use node selectors to target appropriate nodes

### Performance Optimization

**Box Reuse:**

```python
# Create pool of boxes
boxes = [
    runtime.create(boxlite.BoxOptions(image="python:slim"))
    for _ in range(10)
]

# Reuse boxes for multiple tasks
for i, task in enumerate(tasks):
    box = boxes[i % len(boxes)]
    await box.exec("python", "-c", task.code)
```

**Image Caching:**

```python
# Pre-pull images before high traffic
images = ["python:slim", "node:alpine", "alpine:latest"]
for image in images:
    runtime.create(boxlite.BoxOptions(image=image))
# Images are now cached in ~/.boxlite/images/
```

**Concurrent Execution:**

```python
import asyncio

async def run_tasks_concurrently(tasks):
    """Run multiple tasks in parallel."""
    async def run_task(task):
        async with boxlite.SimpleBox(image="python:slim") as box:
            return await box.exec("python", "-c", task.code)

    return await asyncio.gather(*[run_task(t) for t in tasks])
```
