# BoxLite Node.js SDK Examples

This directory contains comprehensive examples demonstrating how to use the BoxLite Node.js SDK.

## For End Users

If you installed BoxLite via npm:

```bash
# Install BoxLite
npm install boxlite

# Run examples directly
node simplebox.js
node codebox.js
```

## For Developers (Working in the Repo)

If you're developing BoxLite:

```bash
# 1. Build the SDK
cd ../../sdks/node
npm install
npm run build

# 2. Link the package globally
npm link

# 3. Link to examples directory
cd ../../examples/node
npm link boxlite

# 4. Run examples
node simplebox.js
```

## Running Examples

```bash
# Simple command execution
node simplebox.js

# Python code execution
node codebox.js

# Desktop automation (requires X11)
node computerbox.js

# Browser automation
node browserbox.js

# Interactive terminal session
node interactivebox.js
```

## Examples Overview

### simplebox.js
Basic container usage:
- Creating containers
- Executing commands
- Collecting output
- Handling errors
- Cleanup

### codebox.js
Python code execution:
- Running Python code safely
- Installing packages
- Using popular libraries (requests, numpy, etc.)
- Error handling

### computerbox.js
Desktop automation:
- GUI environment with web access
- Mouse automation (move, click, drag)
- Keyboard automation (type, key combinations)
- Screenshots
- Scrolling

Access the desktop via browser:
- HTTP: `http://localhost:3000`
- HTTPS: `https://localhost:3001` (self-signed certificate)

### browserbox.js
Browser automation:
- Starting browsers with remote debugging
- Chrome DevTools Protocol (CDP)
- Integration with Puppeteer/Playwright

Optional: Install Puppeteer for full example:
```bash
npm install puppeteer-core
```

### interactivebox.js
Interactive terminal sessions:
- PTY-based interactive shells
- Real-time I/O forwarding
- Terminal size auto-detection

## Tips

1. **First Run**: Image pulls may take time. Subsequent runs are faster.

2. **Resource Limits**: Adjust `memoryMib` and `cpus` based on your system:
   ```javascript
   const box = new SimpleBox({
     image: 'alpine:latest',
     memoryMib: 512,   // Memory in MiB
     cpus: 1           // Number of CPU cores
   });
   ```

3. **Error Handling**: Always use try/finally for cleanup:
   ```javascript
   const box = new SimpleBox({ image: 'alpine:latest' });
   try {
     await box.exec('echo', 'hello');
   } finally {
     await box.stop();  // Important: cleanup resources
   }
   ```

4. **Async Disposal** (TypeScript 5.2+):
   ```typescript
   await using box = new SimpleBox({ image: 'alpine:latest' });
   await box.exec('echo', 'hello');
   // Automatically cleaned up
   ```

## Troubleshooting

**"BoxLite native extension not found"**
- Run `npm run build` in the parent directory

**"Image not found"**
- BoxLite will auto-pull images on first use
- Ensure you have internet connectivity

**"Permission denied" on Linux**
- Check KVM access: `ls -l /dev/kvm`
- Add user to kvm group: `sudo usermod -aG kvm $USER`

## Next Steps

- See [../README.md](../README.md) for full API documentation
- Check [../../docs/](../../docs/) for architecture details
- Join our community for support
