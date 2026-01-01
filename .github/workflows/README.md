# BoxLite CI/CD Workflows

This directory contains GitHub Actions workflows for building and publishing BoxLite SDKs.

## Build Optimization Strategy

### Problem

Building BoxLite SDKs requires:
1. **BoxLite Runtime** - Platform-specific binaries (boxlite-guest, boxlite-shim, dylibs)
2. **Python SDK** - Compiled extension using PyO3
3. **Node.js SDK** - Compiled addon using napi-rs

The runtime build is expensive (~5-10 minutes) and **identical** for both Python and Node.js SDKs on the same platform.

### Solution: Artifact Reuse

Instead of building the runtime twice per platform, we:

1. **Build Runtime Once** (Job: `build-runtime`)
   - Runs for each platform (macOS ARM64, Linux x64, Linux ARM64)
   - Builds `target/boxlite-runtime/` containing:
     - `boxlite-guest` - VM guest agent
     - `boxlite-shim` - Process isolation shim
     - `debugfs`, `mke2fs` - Filesystem tools
     - `libkrun`, `libkrunfw`, `libgvproxy` - Hypervisor libraries
   - Uploads as GitHub Actions artifact

2. **Build Python SDK** (Job: `build-python`)
   - Downloads runtime artifact for platform
   - Builds Python wheels with `cibuildwheel`
   - Reuses runtime instead of rebuilding

3. **Build Node.js SDK** (Job: `build-node`)
   - Downloads runtime artifact for platform
   - Builds Node.js addon with `napi-rs`
   - Reuses runtime instead of rebuilding

### Time Savings

**Before (sequential):**
```
Runtime build: 8 min
Python build:  5 min (includes runtime rebuild)
Node build:    5 min (includes runtime rebuild)
Total:        18 min per platform
```

**After (parallel with reuse):**
```
Runtime build: 8 min
Python build:  2 min (reuses artifact) ┐
Node build:    2 min (reuses artifact) ┘ Parallel
Total:        10 min per platform
```

**Savings: ~45% faster** (18min → 10min per platform)

## Workflows

### `build-sdks.yml`

Main workflow for building and publishing SDKs.

**Triggers:**
- Push to `main` or `develop` branches
- Pull requests to `main`
- Tags matching `v*` (e.g., `v0.1.0`)
- Manual workflow dispatch

**Jobs:**

1. **`build-runtime`** - Build BoxLite runtime
   - Matrix: macOS ARM64, Linux x64, Linux ARM64
   - Outputs: Runtime artifacts per platform

2. **`build-python`** - Build Python wheels
   - Depends on: `build-runtime`
   - Uses: Downloaded runtime artifacts
   - Outputs: Python wheels for Python 3.10, 3.11, 3.12

3. **`build-node`** - Build Node.js packages
   - Depends on: `build-runtime`
   - Uses: Downloaded runtime artifacts
   - Outputs: Node.js native addons per platform

4. **`publish-pypi`** - Publish to PyPI
   - Runs on: Tags (`v*`)
   - Requires: `PYPI_API_TOKEN` secret

5. **`publish-npm`** - Publish to npm
   - Runs on: Tags (`v*`)
   - Requires: `NPM_TOKEN` secret
   - Status: Not yet implemented (needs platform package strategy)

## Local Development

For local development, use the Makefile targets:

```bash
# Build runtime once (debug mode)
make runtime-debug

# Build Python SDK (reuses runtime)
make dev:python

# Build Node.js SDK (reuses runtime)
make dev:node
```

The Makefile automatically copies `target/boxlite-runtime/` to the SDK directories.

## Runtime Artifacts

Each platform's runtime artifact contains:

**macOS ARM64:**
```
boxlite-runtime/
├── boxlite-guest          (158 MB) - VM guest agent
├── boxlite-shim           ( 11 MB) - Process shim
├── debugfs                (680 KB) - Filesystem debugger
├── mke2fs                 (580 KB) - ext4 formatter
├── libgvproxy.dylib       ( 11 MB) - Network proxy
├── libkrun.1.15.1.dylib   (  4 MB) - Hypervisor.framework wrapper
└── libkrunfw.4.dylib      ( 22 MB) - Firmware/kernel
```

**Linux x64/ARM64:**
```
boxlite-runtime/
├── boxlite-guest          (158 MB) - VM guest agent
├── boxlite-shim           ( 11 MB) - Process shim
├── debugfs                (680 KB) - Filesystem debugger
├── mke2fs                 (580 KB) - ext4 formatter
├── libgvproxy.so          ( 11 MB) - Network proxy
├── libkrun.so.1.15.1      (  4 MB) - KVM wrapper
└── libkrunfw.so.4         ( 22 MB) - Firmware/kernel
```

Total size: ~207 MB per platform

## Adding New Platforms

To add a new platform (e.g., Windows WSL):

1. Add platform to `build-runtime` matrix:
   ```yaml
   - name: windows-x64
     os: windows-2022
     target: x86_64-pc-windows-msvc
     artifact_name: runtime-windows-x64
   ```

2. Add platform to `build-python` and `build-node` matrices

3. Update platform-specific build steps if needed

## Secrets Required

- `PYPI_API_TOKEN` - PyPI API token for publishing Python wheels
- `NPM_TOKEN` - npm access token for publishing Node.js packages

Set these in repository Settings → Secrets and variables → Actions.

## Troubleshooting

**Artifact not found:**
- Check `build-runtime` job completed successfully
- Verify artifact retention (default: 7 days)
- Check artifact name matches between upload/download

**Runtime binaries missing:**
- Ensure `make runtime` completes without errors
- Check `target/boxlite-runtime/` contains all files
- Verify submodules are initialized (`git submodule update --init --recursive`)

**Cross-compilation failures (Linux ARM64):**
- Install cross-compilation toolchain: `gcc-aarch64-linux-gnu`
- Set `RUST_TARGET=aarch64-unknown-linux-gnu`
- Use `cargo build --target aarch64-unknown-linux-gnu`

## Future Optimizations

1. **Cache Rust dependencies** - Use `actions/cache` for `~/.cargo`
2. **Incremental builds** - Cache `target/` directory
3. **Matrix parallelization** - Build all platforms simultaneously
4. **Docker layer caching** - For manylinux builds
5. **Separate guest binary** - Cache guest separately (largest artifact)

## References

- [GitHub Actions Artifacts](https://docs.github.com/en/actions/using-workflows/storing-workflow-data-as-artifacts)
- [cibuildwheel](https://cibuildwheel.readthedocs.io/)
- [napi-rs](https://napi.rs/)
- [PyO3](https://pyo3.rs/)
