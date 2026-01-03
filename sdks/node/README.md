# BoxLite Node.js/TypeScript SDK

Embeddable VM runtime for secure, isolated code execution environments.

**Think "SQLite for sandboxing"** - a lightweight library embedded directly in your application without requiring a daemon or root privileges.

[![npm version](https://badge.fury.io/js/%40boxlite%2Fcore.svg)](https://badge.fury.io/js/boxlite)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

## Features

- **Hardware-level VM isolation** (KVM on Linux, Hypervisor.framework on macOS)
- **OCI container support** - Use any Docker/OCI image
- **Async-first API** - Built on native async Rust + napi-rs
- **Multiple specialized boxes**:
  - `SimpleBox` - Basic command execution
  - `CodeBox` - Python code sandbox
  - `BrowserBox` - Browser automation (Puppeteer/Playwright)
  - `ComputerBox` - Desktop automation (14 automation functions)
  - `InteractiveBox` - PTY terminal sessions
- **Zero daemon** - No background processes required
- **Cross-platform** - macOS ARM64, Linux x86_64/ARM64

## Installation

```bash
npm install boxlite
```

**Requirements:**
- Node.js 18 or later
- Platform with hardware virtualization:
  - macOS: Apple Silicon (macOS 12+)
  - Linux: x86_64 or ARM64 with KVM (`/dev/kvm` accessible)

**Not supported:** macOS Intel, Windows (use WSL2 with Linux requirements)

## Quick Start

### JavaScript

```javascript
const { SimpleBox } = require('boxlite');

async function main() {
  const box = new SimpleBox({ image: 'alpine:latest' });

  try {
    const result = await box.exec('echo', 'Hello from BoxLite!');
    console.log(result.stdout);  // Hello from BoxLite!
  } finally {
    await box.stop();
  }
}

main();
```

### TypeScript

```typescript
import { SimpleBox } from 'boxlite';

async function main() {
  const box = new SimpleBox({ image: 'alpine:latest' });

  try {
    const result = await box.exec('echo', 'Hello from BoxLite!');
    console.log(result.stdout);
  } finally {
    await box.stop();
  }
}

main();
```

### TypeScript 5.2+ (Async Disposal)

```typescript
import { SimpleBox } from 'boxlite';

async function main() {
  await using box = new SimpleBox({ image: 'alpine:latest' });
  const result = await box.exec('echo', 'Hello!');
  console.log(result.stdout);
  // Box automatically stopped when leaving scope
}

main();
```

## API Reference

### SimpleBox

Basic container for command execution.

```typescript
import { SimpleBox } from 'boxlite';

const box = new SimpleBox({
  image: 'python:slim',
  memoryMib: 512,    // Memory limit in MiB
  cpus: 2,           // Number of CPU cores
  name: 'my-box',    // Optional name
  autoRemove: true,  // Auto-remove on stop (default: true)
  workingDir: '/app',
  env: { FOO: 'bar' },
  volumes: [
    { hostPath: '/tmp/data', guestPath: '/data', readOnly: false }
  ],
  ports: [
    { hostPort: 8080, guestPort: 80 }
  ]
});

// Execute command
const result = await box.exec('ls', '-la', '/');
console.log(result.exitCode, result.stdout, result.stderr);

// Get box info
console.log(box.id);    // ULID
console.log(box.name);  // Optional name
console.log(box.info()); // Metadata

// Cleanup
await box.stop();
```

### CodeBox

Python code execution sandbox.

```typescript
import { CodeBox } from 'boxlite';

const codebox = new CodeBox({
  image: 'python:slim',  // default
  memoryMib: 512,
  cpus: 1
});

try {
  // Run Python code
  const result = await codebox.run(`
import math
print(f"Pi is approximately {math.pi}")
  `);
  console.log(result);  // Pi is approximately 3.141592653589793

  // Install packages
  await codebox.installPackage('requests');
  await codebox.installPackages('numpy', 'pandas');

  // Use installed packages
  const result2 = await codebox.run(`
import requests
response = requests.get('https://api.github.com/zen')
print(response.text)
  `);
  console.log(result2);
} finally {
  await codebox.stop();
}
```

### BrowserBox

Browser automation with remote debugging.

```typescript
import { BrowserBox } from 'boxlite';

const browser = new BrowserBox({
  browser: 'chromium',  // 'chromium', 'firefox', or 'webkit'
  memoryMib: 2048,
  cpus: 2
});

try {
  await browser.start();

  // Get CDP endpoint
  const endpoint = browser.endpoint();
  console.log(endpoint);  // http://localhost:9222

  // Connect with Puppeteer
  const puppeteer = require('puppeteer-core');
  const browserInstance = await puppeteer.connect({ browserURL: endpoint });

  const page = await browserInstance.newPage();
  await page.goto('https://example.com');
  const title = await page.title();
  console.log(title);

  await page.close();
} finally {
  await browser.stop();
}
```

### ComputerBox

Desktop automation with web access.

```typescript
import { ComputerBox } from 'boxlite';

const desktop = new ComputerBox({
  cpus: 2,
  memoryMib: 2048,
  guiHttpPort: 3000,   // default
  guiHttpsPort: 3001   // default (self-signed cert)
});

try {
  // Wait for desktop to be ready
  await desktop.waitUntilReady(60);

  // Mouse automation
  await desktop.mouseMove(100, 200);
  await desktop.leftClick();
  await desktop.doubleClick();
  await desktop.rightClick();
  await desktop.leftClickDrag(100, 100, 200, 200);

  const [x, y] = await desktop.cursorPosition();
  console.log(`Cursor at: ${x}, ${y}`);

  // Keyboard automation
  await desktop.type('Hello, World!');
  await desktop.key('Return');
  await desktop.key('ctrl+c');

  // Screenshot
  const screenshot = await desktop.screenshot();
  console.log(`${screenshot.width}x${screenshot.height} ${screenshot.format}`);
  // screenshot.data contains base64-encoded PNG

  // Scroll
  await desktop.scroll(500, 300, 'down', 5);

  // Get screen size
  const [width, height] = await desktop.getScreenSize();
  console.log(`Screen: ${width}x${height}`);

  // Access desktop via browser:
  // HTTP:  http://localhost:3000
  // HTTPS: https://localhost:3001 (self-signed certificate)
} finally {
  await desktop.stop();
}
```

### InteractiveBox

Interactive terminal sessions with PTY.

```typescript
import { InteractiveBox } from 'boxlite';

const box = new InteractiveBox({
  image: 'alpine:latest',
  shell: '/bin/sh',
  tty: true,  // Auto-detected if undefined
  memoryMib: 512,
  cpus: 1
});

try {
  await box.start();
  await box.wait();  // Blocks until shell exits
} finally {
  await box.stop();
}
```

## Examples

See [../../examples/node/](../../examples/node/) directory for complete examples:

```bash
# If installed via npm
npm install boxlite
node simplebox.js

# If working from source
cd ../../sdks/node
npm install && npm run build
npm link
cd ../../examples/node
npm link boxlite
node simplebox.js
```

## Error Handling

```typescript
import { SimpleBox, ExecError, TimeoutError, ParseError } from 'boxlite';

try {
  const box = new SimpleBox({ image: 'alpine:latest' });
  await box.exec('false');  // Exit code 1
} catch (err) {
  if (err instanceof ExecError) {
    console.error(`Command failed: ${err.command}`);
    console.error(`Exit code: ${err.exitCode}`);
    console.error(`Stderr: ${err.stderr}`);
  }
}
```

## Building from Source

```bash
# Clone repository
git clone https://github.com/boxlite-labs/boxlite.git
cd boxlite/sdks/node

# Initialize submodules (critical!)
git submodule update --init --recursive

# Install dependencies
npm install

# Build (Rust + TypeScript)
npm run build

# Link to examples
npm link
cd ../../examples/node
npm link boxlite

# Run examples
node simplebox.js
```

## TypeScript Support

Full TypeScript support included:

```typescript
import {
  SimpleBox, CodeBox, BrowserBox, ComputerBox, InteractiveBox,
  type SimpleBoxOptions,
  type CodeBoxOptions,
  type BrowserBoxOptions,
  type ComputerBoxOptions,
  type InteractiveBoxOptions,
  type ExecResult,
  type Screenshot,
  type BrowserType
} from 'boxlite';
```

## Platform Requirements

### macOS
- Apple Silicon (ARM64)
- macOS 12+ (Monterey or later)
- Hypervisor.framework (built-in)

### Linux
- x86_64 or ARM64
- KVM enabled (`/dev/kvm` accessible)
- User in `kvm` group:
  ```bash
  sudo usermod -aG kvm $USER
  # Logout/login required
  ```

## Architecture

The SDK uses a dual-layer architecture:

1. **Layer 1 (Rust)**: napi-rs bindings to native BoxLite runtime
   - Direct mapping to Rust `boxlite` crate
   - Async operations via `env.spawn()` (non-blocking)
   - Stream handling via async iterators

2. **Layer 2 (TypeScript)**: Convenience wrappers
   - Specialized box classes (CodeBox, BrowserBox, etc.)
   - Error class hierarchy
   - Output collection helpers
   - TypeScript type definitions

## Performance

- Startup time: < 100ms
- Overhead vs Python SDK: < 5%
- Concurrent boxes: Limited by system resources
- Memory footprint: ~50-100MB per box (depends on image)

## Troubleshooting

**"BoxLite native extension not found"**
- Run `npm run build` to compile Rust bindings

**"Image not found"**
- BoxLite auto-pulls OCI images on first use
- Ensure internet connectivity

**"Permission denied" (Linux)**
- Check KVM access: `ls -l /dev/kvm`
- Add user to kvm group: `sudo usermod -aG kvm $USER`

**"Unsupported engine"**
- Only Apple Silicon Macs supported (not Intel)
- Windows users: Use WSL2 with Linux requirements

## Contributing

See [../../CONTRIBUTING.md](../../CONTRIBUTING.md) for development guidelines.

## License

Apache 2.0 - See [../../LICENSE](../../LICENSE)

## Related Projects

- [Python SDK](../python/) - Python bindings (stable, v0.4.4)
- [C SDK](../c/) - C FFI bindings (early stage)
- [Core Runtime](../../boxlite/) - Rust runtime implementation

## Support

- **Issues**: [GitHub Issues](https://github.com/boxlite-labs/boxlite/issues)
- **Documentation**: [docs/](../../docs/)
- **Examples**: [examples/](./examples/)

---

**Made with ❤️ by BoxLite Labs**
