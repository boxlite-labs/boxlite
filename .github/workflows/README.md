# BoxLite CI/CD Workflows

This directory contains GitHub Actions workflows for building and publishing BoxLite SDKs.

## Workflow Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                         config.yml                                   │
│                    (shared configuration)                            │
└─────────────────────────────────────────────────────────────────────┘
                                │
        ┌───────────────────────┼───────────────────────┐
        ↓                       ↓                       ↓
┌───────────────┐     ┌─────────────────┐     ┌─────────────────┐
│build-runtime  │     │build-python-sdk │     │build-node-sdk   │
│               │     │                 │     │                 │
│ Triggers:     │     │ Triggers:       │     │ Triggers:       │
│ - core/*      │────→│ - sdks/python/* │     │ - sdks/node/*   │
│ - Cargo.*     │     │ - workflow_run  │     │ - workflow_run  │
│               │     │   (after runtime│     │   (after runtime│
│ Saves to:     │     │    completes)   │     │    completes)   │
│ actions/cache │     │                 │     │                 │
└───────────────┘     │ Restores from:  │     │ Restores from:  │
                      │ actions/cache   │     │ actions/cache   │
                      └─────────────────┘     └─────────────────┘
```

## Key Design: Cache-Based Separation

Instead of artifacts (which only work within a single workflow), we use **`actions/cache`** to share runtime builds across workflows:

```yaml
# build-runtime.yml saves:
key: boxlite-runtime-{platform}-{hash of core files}

# build-python-sdk.yml / build-node-sdk.yml restore:
key: boxlite-runtime-{platform}-{hash of core files}
restore-keys: boxlite-runtime-{platform}-  # fallback to latest
```

**Benefits:**
- SDK workflows only rebuild runtime on cache miss
- Same core code = same cache key = instant restore
- Core changes = different hash = new build

## Workflows

### `config.yml`

Shared configuration loaded by all workflows.

**Outputs:**
- `build-targets` - OS targets for testing (`["ubuntu-latest", "macos-15"]`)
- `python-versions` - Python versions (`["3.10", "3.11", "3.12", "3.13"]`)
- `node-versions` - Node.js versions (`["18", "20", "22"]`)
- `sdk-platforms` - Full platform matrix with cross-compilation config

### `build-runtime.yml`

Builds BoxLite runtime and saves to cache.

**Triggers:**
- Push to `main`/`develop` with changes in `boxlite/**`, `Cargo.*`, etc.
- Pull requests with core changes
- Releases
- Manual dispatch

**What it builds:**
- `boxlite-guest` - VM guest agent
- `boxlite-shim` - Process isolation shim
- `libkrun`, `libkrunfw`, `libgvproxy` - Hypervisor libraries
- `debugfs`, `mke2fs` - Filesystem tools

### `build-python-sdk.yml`

Builds, tests, and publishes Python SDK.

**Triggers:**
- Push/PR with changes in `sdks/python/**`
- `workflow_run` after `Build Runtime` completes (automatic rebuild when core changes)
- Releases
- Manual dispatch

**Jobs:**
1. `build` - Builds Python wheels using cibuildwheel
2. `test` - Tests import on Python 3.10-3.13
3. `publish` - Publishes to PyPI (on release)
4. `upload-to-release` - Uploads wheels to GitHub Release

### `build-node-sdk.yml`

Builds, tests, and publishes Node.js SDK.

**Triggers:**
- Push/PR with changes in `sdks/node/**`
- `workflow_run` after `Build Runtime` completes
- Releases
- Manual dispatch

**Jobs:**
1. `build` - Builds Node.js addon with napi-rs
2. `test` - Tests import on Node 18, 20, 22
3. `publish` - Publishes to npm (on release)
4. `upload-to-release` - Uploads tarball to GitHub Release

### `build-wheels.yml` (Legacy)

Original Python-only workflow. Kept for reference.

## Trigger Behavior

| Change | build-runtime | build-python-sdk | build-node-sdk |
|--------|---------------|------------------|----------------|
| `boxlite/**` | ✅ Runs | ✅ Runs (via workflow_run) | ✅ Runs (via workflow_run) |
| `sdks/python/**` | ❌ Skips | ✅ Runs | ❌ Skips |
| `sdks/node/**` | ❌ Skips | ❌ Skips | ✅ Runs |
| Release published | ✅ Runs | ✅ Runs | ✅ Runs |

## Cache Strategy

### Runtime Cache

```yaml
key: boxlite-runtime-{platform}-{hashFiles('boxlite/**', 'Cargo.lock', ...)}
```

- **Same core code** → Cache hit → Skip rebuild (~8 min saved)
- **Core changed** → Cache miss → Rebuild runtime

### Rust Dependencies Cache (Swatinem/rust-cache)

```yaml
shared-key: "runtime-{target}"    # For build-runtime
shared-key: "python-sdk-{target}" # For build-python-sdk
shared-key: "node-sdk-{target}"   # For build-node-sdk
```

- Caches `~/.cargo` and `./target` directories
- Shared across workflow runs
- Invalidates on Cargo.lock changes

## Time Savings

**Scenario: Only Python SDK changed**

| Without separation | With separation |
|-------------------|-----------------|
| Build runtime: 8 min | ❌ Skipped |
| Build Python: 2 min | ✅ 2 min (cache hit) |
| Build Node: 2 min | ❌ Skipped |
| **Total: 12 min** | **Total: 2 min** |

**Savings: 83% faster**

## Secrets Required

- `PYPI_API_TOKEN` - PyPI API token for publishing Python wheels
- `NPM_TOKEN` - npm access token for publishing Node.js packages

Set these in repository Settings → Secrets and variables → Actions.

## Local Development

```bash
# Build runtime once
make runtime

# Build Python SDK (reuses runtime)
make dev:python

# Build Node.js SDK (reuses runtime)
make dev:node
```

## Troubleshooting

**Cache miss when expected hit:**
- Check if core files changed (hash is different)
- Caches expire after 7 days of non-use
- Branch-based cache isolation may apply

**workflow_run not triggering:**
- Only triggers on `completed` (not `success`)
- Check that base workflow is on watched branches
- `check-trigger` job skips if runtime failed

**Runtime binaries missing:**
- Fallback build runs automatically on cache miss
- Check logs for "Runtime cache miss - building runtime"
- Verify submodules initialized

## References

- [GitHub Actions Cache](https://github.com/actions/cache)
- [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache)
- [workflow_run trigger](https://docs.github.com/en/actions/using-workflows/events-that-trigger-workflows#workflow_run)
- [cibuildwheel](https://cibuildwheel.readthedocs.io/)
- [napi-rs](https://napi.rs/)
