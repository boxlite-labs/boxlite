# BoxLite Python SDK Examples

This directory contains comprehensive examples demonstrating how to use the BoxLite Python SDK.

## For End Users

If you installed BoxLite via pip:

```bash
# Install BoxLite
pip install boxlite

# Run examples directly
python simplebox_example.py
python codebox_example.py
```

## For Developers (Working in the Repo)

If you're developing BoxLite:

```bash
# 1. Build the SDK
cd ../../sdks/python
pip install -e .

# 2. Run examples
cd ../../examples/python
python simplebox_example.py
```

## Running Examples

```bash
# Simple command execution
python simplebox_example.py

# Python code execution
python codebox_example.py

# Desktop automation (requires X11)
python computerbox_example.py

# Browser automation
python browserbox_example.py

# Interactive terminal session
python interactivebox_example.py

# Lifecycle management
python lifecycle_example.py

# Cross-process communication
python cross_process_example.py

# List all boxes
python list_boxes_example.py

# Low-level native API
python native_example.py
```

## Examples Overview

### simplebox_example.py
Foundation for custom containers:
- Command execution with results
- Separate stdout and stderr handling
- Environment variables and working directory
- Error handling and exit codes
- Multiple commands in same container

### codebox_example.py
Secure Python code execution:
- Running untrusted Python code safely
- Installing packages dynamically
- Using popular libraries (requests, numpy, etc.)
- Real-world use case: AI agent code execution

### computerbox_example.py
Desktop automation:
- GUI environment with web access
- Mouse automation (move, click, drag)
- Keyboard automation (type, key combinations)
- Screenshots
- Web browser access

Access the desktop via browser:
- HTTP: `http://localhost:3000`
- HTTPS: `https://localhost:3001` (self-signed certificate)

### browserbox_example.py
Browser automation:
- Starting different browsers (chromium, firefox, webkit)
- Chrome DevTools Protocol (CDP) endpoints
- Integration with Puppeteer/Playwright
- Cross-browser testing

Optional: Install Playwright for full example:
```bash
pip install playwright
playwright install chromium
```

### interactivebox_example.py
Interactive terminal sessions:
- PTY-based interactive shells
- Real-time I/O forwarding
- Terminal size auto-detection
- Similar to `docker exec -it`

### lifecycle_example.py
Complete lifecycle management:
- Creating, starting, stopping boxes
- Resource monitoring (CPU, memory, disk)
- Persistent boxes (named, survives restarts)
- Box metadata and state tracking

### cross_process_example.py
Multi-process coordination:
- Creating boxes in one process
- Attaching from other processes
- Shared box access patterns
- Cross-process communication

### list_boxes_example.py
Box management:
- Listing all active boxes
- Filtering by status
- Box metadata inspection

### native_example.py
Low-level native API:
- Direct Rust API access
- Advanced configuration
- Performance-critical use cases

## Tips

1. **First Run**: Image pulls may take time. Subsequent runs are faster.

2. **Resource Limits**: Adjust `memory_mib` and `cpus` based on your system:
   ```python
   box = boxlite.SimpleBox(
       image='alpine:latest',
       memory_mib=512,   # Memory in MiB
       cpus=1            # Number of CPU cores
   )
   ```

3. **Error Handling**: Always use async context managers for cleanup:
   ```python
   async with boxlite.SimpleBox(image='alpine:latest') as box:
       result = await box.exec('echo', 'hello')
   # Automatically cleaned up
   ```

4. **Logging**: Enable debug logging to troubleshoot:
   ```python
   import logging
   logging.basicConfig(level=logging.DEBUG)
   ```

## Troubleshooting

**"BoxLite runtime not found"**
- Run `pip install boxlite` or `pip install -e .` from `sdks/python`

**"Image not found"**
- BoxLite will auto-pull images on first use
- Ensure you have internet connectivity

**"Permission denied" on Linux**
- Check KVM access: `ls -l /dev/kvm`
- Add user to kvm group: `sudo usermod -aG kvm $USER`
- Logout and login for group changes to take effect

**"UnsupportedEngine" on macOS Intel**
- Intel Macs are not supported (Hypervisor.framework stability issues)
- Use Apple Silicon (ARM64) instead or Linux with KVM

## Next Steps

- See [../../sdks/python/README.md](../../sdks/python/README.md) for full API documentation
- Check [../../docs/](../../docs/) for architecture details
- Browse [../../README.md](../../README.md) for project overview
