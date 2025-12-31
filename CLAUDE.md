# CLAUDE.md

This document provides project context for AI-assisted development with Claude Code.

## Project Overview

**BoxLite** is an embeddable virtual machine runtime for secure, isolated code execution environments.

**Key Concept:** Think "SQLite for sandboxing" - a lightweight library embedded directly in applications without requiring a daemon or root privileges.

**What it does:**
- Hardware-level VM isolation (KVM on Linux, Hypervisor.framework on macOS)
- Runs OCI containers inside lightweight VMs
- Async-first API for running multiple isolated environments concurrently

**Primary use cases:**
- AI Agent Sandbox - Safe execution environment for untrusted AI-generated code
- Serverless Multi-tenant Runtime - Isolated environments per customer/request
- Regulated Environments - Hardware-level isolation for compliance

**For detailed overview, architecture, and use cases, see:**
- [README.md](./README.md) - Project overview, features, quick start
- [docs/architecture/README.md](./docs/architecture/README.md) - Comprehensive architecture documentation

## Tech Stack

**Core:**
- Rust (async, Tokio runtime)
- libkrun hypervisor (KVM/Hypervisor.framework)
- libcontainer (OCI runtime)
- gRPC (tonic) for host-guest communication

**SDKs:**
- Python (PyO3, stable v0.4.4)
- C (FFI, early stage)
- Node.js (napi-rs, work in progress)

**For complete tech stack details, see:**
- [docs/architecture/README.md](./docs/architecture/README.md#tech-stack) - Detailed component breakdown

## Project Structure

**High-level layout:**
```
boxlite/              # Core runtime (Rust) - 19 modules
boxlite-shared/       # Shared types and protocol
guest/                # Guest agent (runs inside VM)
sdks/
  python/             # Python SDK (PyO3, v0.4.4)
  c/                  # C SDK (FFI)
  node/               # Node.js SDK (WIP)
examples/python/      # 9 comprehensive Python examples
docs/                 # Documentation
scripts/              # Build and setup scripts
```

**Key modules in `boxlite/src/`:**
- `runtime/` - BoxliteRuntime, main entry point
- `litebox/` - LiteBox handle, command execution
- `vmm/` - VM manager (libkrun, shim controller)
- `portal/` - Host-guest gRPC communication
- `images/` - OCI image management
- `net/`, `volumes/`, `disk/` - Networking, storage, disks
- `db/` - SQLite persistence

**Runtime data directory (`~/.boxlite/`):**
- `images/` - OCI image cache
- `boxes/` - Per-box data (config, disks)
- `db/` - SQLite databases

**For detailed project structure, see:**
- [CONTRIBUTING.md](./CONTRIBUTING.md#project-structure) - Directory layout and organization
- [docs/architecture/README.md](./docs/architecture/README.md) - Component architecture

## Common Commands

**Quick reference:**
```bash
make setup          # Install dependencies (auto-detects OS)
make dev:python     # Build Python SDK locally
make test           # Run Rust tests
make fmt            # Format code
make dist:python    # Build portable Python wheel
```

**For complete command reference and Makefile targets, see:**
- [Makefile](./Makefile) - All available targets with descriptions (run `make help`)
- [CONTRIBUTING.md](./CONTRIBUTING.md#building-from-source) - Development workflow
- [docs/guides/README.md](./docs/guides/README.md#building-from-source) - Platform-specific build instructions

## Code Style

**Rust:**
- `cargo fmt` for formatting (enforced in CI)
- `cargo clippy` for linting
- Async-first (Tokio runtime)
- Error handling via centralized `BoxliteError` enum
- `Send + Sync` for public types

**Python:**
- Async/await for all I/O
- Context managers (`async with`) for automatic cleanup
- Type hints encouraged

**For complete code style guidelines, see:**
- [CONTRIBUTING.md](./CONTRIBUTING.md#code-style) - Detailed style guidelines
- [boxlite-shared/src/errors.rs](./boxlite-shared/src/errors.rs) - Error handling patterns

## Workflows

**Development workflow:**
1. Create feature branch from `main`
2. Make changes, add tests, update docs
3. Run `cargo test && make fmt`
4. Open Pull Request
5. Address code review feedback
6. Squash merge to main

**Testing:**
- Rust: `cargo test` (unit + integration)
- Python: `pytest` with real VM integration tests

**For complete workflows, see:**
- [CONTRIBUTING.md](./CONTRIBUTING.md#how-to-contribute) - PR process, testing, release
- [.github/workflows/](../.github/workflows/) - CI/CD pipelines

## Important Notes

### Critical for AI Assistants

**Platform support:**
- ✅ macOS ARM64 (Apple Silicon), Linux x86_64/ARM64
- ❌ macOS Intel, Windows (not supported)

**Before building:**
- ⚠️ **MUST** run `git submodule update --init --recursive` (vendored dependencies)
- Check platform requirements in [README.md](./README.md#supported-platforms)

**Architecture quirks:**
- **Shim process**: boxlite-shim isolates boxes (libkrun does process takeover)
- **gRPC communication**: Host-guest communication via vsock (not TCP)
- **~/.boxlite directory**: All runtime data stored here (images, boxes, db)

**When writing code:**
- All I/O is async (Tokio runtime)
- Errors use centralized `BoxliteError` enum (see `boxlite-shared/src/errors.rs`)
- Python SDK version: 0.4.4 (requires Python 3.10+)
- Examples: 9 comprehensive Python examples in `examples/python/`

**Common pitfalls:**
- Forgetting to initialize submodules → build fails
- Running on Intel Mac → UnsupportedEngine error
- Not handling async/await properly → runtime errors
- Exceeding resource limits → box killed (OOM)

### Documentation Map

When users ask questions, direct them to:

| Question Type | Reference |
|--------------|-----------|
| "How do I install?" | [docs/getting-started/README.md](./docs/getting-started/README.md) |
| "How does it work?" | [docs/architecture/README.md](./docs/architecture/README.md) |
| "What can I configure?" | [docs/reference/README.md](./docs/reference/README.md) |
| "How do I...?" | [docs/guides/README.md](./docs/guides/README.md) |
| "Why is it failing?" | [docs/faq.md](./docs/faq.md) |
| "Python API reference" | [sdks/python/README.md](./sdks/python/README.md) |
| "How to contribute?" | [CONTRIBUTING.md](./CONTRIBUTING.md) |

### Quick Debugging

When helping users debug:
1. Enable logging: `RUST_LOG=debug python script.py`
2. Check `~/.boxlite/` disk space and permissions
3. Verify platform support (KVM on Linux, Hypervisor.framework on macOS)
4. See [docs/faq.md](./docs/faq.md#troubleshooting) for common issues

## Mandatory Code Design Rules

**CRITICAL: These rules are MANDATORY for all code contributions.**

### Meta-Principle

**0. DON'T BE YES MAN** - Challenge assumptions, question designs, identify flaws

- **Challenge yourself too, not just the user**
- Before implementing: "Does this already exist?" (search first)
- Before adding logic: "Does this layer need to know this?"
- After writing: "What breaks if I delete this?"

### Core Principles (Must Know)

**1. Single Responsibility** - One function, one job
**2. Boring Code** - Obvious > clever
**3. Search Before Implement** - grep before writing, read before coding
**4. Only What's Used** - No future-proofing, delete dead code immediately
**5. DRY** - Don't Repeat Yourself (single source of truth)
**6. Explicit Errors** - Self-documenting error messages

### Supporting Principles (Reference When Needed)

**7. Minimal Knowledge** - Components only know interfaces, not internals
**8. No Premature Optimization** - Measure first, optimize later
**9. Explicit Paths** - Calculate from known roots, never assume
**10. Prepare Before Execute** - Setup before irreversible operations
**11. Validate Early** - Check preconditions before expensive work
**12. Thoughtful Naming** - Consider 5+ alternatives, choose the clearest
**13. Structured Code** - Organized hierarchy, clear layers, predictable organization
**14. Idiomatic by Default** - Follow each language's standard patterns and libraries first; only diverge with a clear, documented reason

### Quick Examples

**DON'T BE YES MAN**

Critical thinking is mandatory. Challenge assumptions, identify flaws, propose better alternatives.

```
// ❌ Yes Man behavior
User: "Let's add a Supervisor component to track metrics"
AI: "Great idea! I'll implement it right away."
[Creates empty struct with no implementation, violates YAGNI]

// ✅ Critical thinking
User: "Let's add a Supervisor component to track metrics"
AI: "Wait - what data would Supervisor need? The process handle is in ShimController.
     Creating a separate component means duplicating PID tracking (violates DRY).
     Supervisor has no access to the data it needs. This design won't work.

     Better approach: ShimController already owns the process, it should provide metrics.
     Do you want me to implement metrics() in ShimController instead?"
```

**Real example from this codebase:**

- Initial design had empty `Supervisor` struct that did nothing
- Yes-man would have kept it or implemented it wrong
- Critical analysis identified: violates YAGNI, DRY, and "Delete Legacy Code"
- Solution: Delete Supervisor, add metrics to ShimController (owns the process)

**When to push back:**

- Design violates any of the 15 golden rules
- User requests create duplicate state/logic
- Implementation requires blocking operations (sleep, sync I/O in async contexts)
- Code adds complexity without clear benefit
- "Future-proofing" that isn't needed now (YAGNI)

**How to push back:**

1. Identify specific rule violations
2. Explain why current approach won't work
3. Propose concrete alternative
4. Let user decide

**Remember:** Respectful disagreement > harmful agreement. Your job is to build good code, not to please.

**Single Responsibility**

```rust
// ❌ One function doing everything
fn setup_and_start_vm(image: &str) -> Result<VM> { /* ... */ }

// ✅ Each function has one job
fn pull_image(image: &str) -> Result<Manifest> { /* ... */ }
fn create_workspace(manifest: &Manifest) -> Result<Workspace> { /* ... */ }
fn start_vm(workspace: &Workspace) -> Result<VM> { /* ... */ }
```

**Boring Code**

```rust
// ❌ Clever, hard to understand
fn metrics(&self) -> RawMetrics {
    self.process.as_ref()
        .and_then(|p| System::new().process(Pid::from(p.id())))
        .map(|proc| RawMetrics { cpu: proc.cpu_usage(), mem: proc.memory() })
        .unwrap_or_default()
}

// ✅ Boring, obvious
fn metrics(&self) -> RawMetrics {
    if let Some(ref process) = self.process {
        let mut sys = System::new();
        sys.refresh_process(pid);
        if let Some(proc_info) = sys.process(pid) {
            return RawMetrics {
                cpu_percent: Some(proc_info.cpu_usage()),
                memory_bytes: Some(proc_info.memory()),
            };
        }
    }
    RawMetrics::default()
}
```

**DRY (Don't Repeat Yourself)**

```rust
// ❌ Duplicated constants
const VSOCK_PORT: u32 = 2695;  // host
const VSOCK_PORT: u32 = 2695;  // guest

// ✅ Shared in boxlite-core
use boxlite_core::VSOCK_GUEST_PORT;
```

**Search Before Implement**

BEFORE writing ANY code, search for existing implementations:

```bash
# ❌ Writing transformation without searching
# (adds duplicate unix→vsock transformation in litebox.rs)

# ✅ Search first, find existing code
$ grep -r "transform.*guest" boxlite/src/
boxlite/src/engines/krun/engine.rs:113:fn transform_guest_args(...)
# → Found it! Use existing code, don't duplicate.
```

```rust
// ❌ Duplicate logic in wrong layer
impl BoxInitializer {
    fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
        // Why does litebox know about krun's unix→vsock transformation?
        let guest_transport = match transport {
            Transport::Unix { .. } => Transport::vsock(2695),  // Already in krun/engine.rs!
            ...
        };
    }
}

// ✅ Let existing engine code handle it
impl BoxInitializer {
    fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
        // Pass transport as-is, krun engine will transform if needed
        let uri = transport.to_uri();
        format!("exec boxlite-guest --listen {}", uri)
    }
}

// Engine already has the transformation (discovered via grep)
impl EngineKrun {
    fn transform_guest_args(args: Vec<String>) -> Vec<String> {
        // Krun-specific: unix:// → vsock:// transformation
    }
}
```

**Search patterns to try:**

- Similar functionality: `grep -r "transform.*args" src/`
- Function names: `grep -r "function_name" src/`
- Constants/config: `grep -r "VSOCK_PORT\|2695" src/`
- Layer ownership: `grep -r "GUEST_AGENT" src/` (shows which modules use it)

**Explicit Path Calculation**

```rust
// ❌ Assumes relationship
let box_dir = rootfs_dir.join(box_id);

// ✅ Calculate from known root
let home_dir = rootfs_dir.parent().ok_or(...) ?;
let box_dir = home_dir.join(dirs::BOXES_DIR).join(box_id);
```

**Only What's Used**

```rust
// ❌ Adding "future-proof" code
pub struct Supervisor {
    // Empty - we might need this later!
}

// ❌ Keeping "just in case" code
// fn old_extract_layers() { ... }  // Commented out, might need later?

// ✅ Only implement what's needed now
// If Supervisor isn't used, don't create it
// If old code isn't called, delete it (it's in git history)
```

**Prepare Before Execute**

```rust
// ❌ Setup mixed with critical operation
fn start_vm() -> Result<()> {
    let ctx = create_ctx()?;
    ctx.start();  // Process takeover - can't recover from errors!
}

// ✅ All setup before point of no return
std::fs::create_dir_all( & socket_dir) ?;  // Can fail safely
let ctx = create_ctx() ?;                 // Can fail safely
ctx.configure() ?;                        // Can fail safely
ctx.start();                             // Point of no return
```

**Explicit Error Context**

```rust
// ❌ Generic error
std::fs::create_dir_all( & dir) ?;

// ✅ Self-documenting
std::fs::create_dir_all( & socket_dir).map_err( | e| {
BoxliteError::Storage(format ! (
"Failed to create socket directory {}: {}", socket_dir.display(), e
))
}) ?;
```

**Thoughtful Naming**

```rust
// ❌ Unclear, abbreviated
fn proc_data(d: &[u8]) -> Vec<u8> { /* ... */ }
fn get_stuff() -> Result<Thing> { /* ... */ }

// ✅ Clear, self-documenting (considered alternatives first)
// Alternatives considered: handle_layers, merge_layers, combine_layers, stack_layers
fn extract_and_merge_layers(tarballs: &[PathBuf], dest: &Path) -> Result<()> { /* ... */ }

// Alternatives considered: fetch_config, load_config, read_config, get_config
fn parse_container_config(manifest: &Manifest) -> Result<ContainerConfig> { /* ... */ }
```

**Naming Process**:

1. Write down 5+ name candidates
2. Consider: verb clarity, noun specificity, context
3. Ask: "If I read this in 6 months, would it be obvious?"
4. Choose the name that needs the least explanation

**Minimal Knowledge**

```rust
// ❌ Component knows about other's internals
mod krun_engine {
    use crate::networking::constants::GUEST_MAC;

    fn configure_network(&self, socket_path: &str) {
        // Engine directly imports networking constant
        self.ctx.add_net_path(socket_path, GUEST_MAC);
    }
}

// ✅ Component only knows interface
mod krun_engine {
    fn configure_network(&self, socket_path: &str, mac_address: [u8; 6]) {
        // Engine receives mac_address as parameter, doesn't know where it comes from
        self.ctx.add_net_path(socket_path, mac_address);
    }
}

// Backend provides the configuration
mod gvproxy_backend {
    use crate::networking::constants::GUEST_MAC;

    fn endpoint(&self) -> NetworkBackendEndpoint {
        NetworkBackendEndpoint::UnixSocket {
            path: self.socket_path.clone(),
            mac_address: GUEST_MAC,  // Backend knows the constant
        }
    }
}
```

**Why this matters:**

- Loose coupling: Components don't know each other's internals
- Independent evolution: Backend and engine can change independently
- Testability: Components can be tested in isolation

**Anti-pattern: Knowledge Leaks**

```rust
// ❌ litebox.rs knowing about krun's socket bridging
impl BoxInitializer {
    fn create_guest_entrypoint(&self) -> GuestEntrypoint {
        // Why does litebox know about krun's unix→vsock transformation?
        let guest_transport = match transport {
            Transport::Unix { .. } => Transport::vsock(2695),  // ← Krun implementation detail!
            ...
        };
    }
}

// ✅ Let the engine handle its own implementation details
impl BoxInitializer {
    fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
        // Pass transport as-is, let engine transform if needed
        let uri = transport.to_uri();
        format!("exec boxlite-guest --listen {}", uri)
    }
}

// Engine handles the transformation
impl EngineKrun {
    fn transform_guest_args(args: Vec<String>) -> Vec<String> {
        // Krun-specific: unix:// → vsock:// transformation
    }
}
```

**Ask before adding logic:**

- Does THIS component need to know this detail?
- Would removing this knowledge make the system more flexible?
- Is this an implementation detail of a different layer?

**Minimal Knowledge applies to EVERYTHING:**

- ✅ Code (imports, function calls, direct references)
- ✅ **Comments** (what implementation details are revealed)
- ✅ Documentation (API contracts, design docs)

```rust
// ❌ Comment reveals implementation details
fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
    // Pass transport as-is - krun engine will transform unix:// to vsock://
    // (see krun/engine.rs::transform_guest_args)
    // ↑ Why does litebox know krun's transformation logic?
    let uri = transport.to_uri();
}

// ✅ Comment maintains abstraction
fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
    // Engine handles any transport-specific transformations
    // ↑ Generic, no implementation details leaked
    let uri = transport.to_uri();
}
```

**Why comments matter:**

- Comments create **documentation coupling** (if implementation changes, comment is wrong)
- Revealing "how" instead of "why" leaks abstractions
- Future readers learn the wrong patterns

**Structured Code**

```rust
// ❌ Flat, disorganized
mod rootfs {
    pub fn prepare() { ... }
    pub fn extract() { ... }
    pub fn mount() { ... }
    pub fn unmount() { ... }
    pub fn process_whiteouts() { ... }
    pub struct PreparedRootfs {
        ...
    }
    pub struct SimpleRootfs {
        ...
    }
}

// ✅ Hierarchical, organized by responsibility
mod rootfs {
    mod operations;  // Low-level primitives
    mod prepared;    // High-level orchestration (uses operations)
    mod simple;      // Alternative implementation

    pub use operations::{extract_layer_tarball, mount_overlayfs_from_layers};
    pub use prepared::PreparedRootfs;
    pub use simple::SimpleRootfs;
}
```

**Structure Principles**:

1. **Clear Hierarchy**: operations → prepared → public API
2. **Separation of Concerns**: Each module has ONE purpose
3. **Progressive Disclosure**: High-level API first, details hidden
4. **Predictable Organization**: Similar things organized similarly
5. **Explicit Dependencies**: Imports show relationships
6. **Testable Isolation**: Each layer can be tested independently

**File Organization Pattern**:

```
src/
  ├── lib.rs              // Public API only
  ├── errors.rs           // Shared error types
  ├── feature/
  │   ├── mod.rs          // Public interface + re-exports
  │   ├── operations.rs   // Low-level primitives
  │   ├── types.rs        // Feature-specific types
  │   └── impl.rs         // High-level implementation
```

### Pre-Submission Checklist

**Pre-Implementation (BEFORE writing code):**

- [ ] Searched for similar functionality (`grep -r "pattern" src/`)
- [ ] Read ALL files that would be affected (completely, not skimmed)
- [ ] Identified correct layer for new logic (ownership analysis)
- [ ] Verified no duplicate logic exists
- [ ] Questioned: "Does this component need to know this?"
- [ ] Applied Rule #0 to OWN design (not just user's request)

**Meta-Principle:**

- [ ] Design was critically evaluated (not yes-man accepted)

**Core Principles:**

- [ ] Each function has single responsibility (one job)
- [ ] Code is boring and obvious (not clever)
- [ ] Only code that's actually used exists (no future-proofing, no dead code)
- [ ] No duplicated knowledge (DRY - single source of truth)
- [ ] Every error has full context (self-documenting)

**Supporting Principles:**

- [ ] Components only know interfaces (minimal knowledge / loose coupling)
- [ ] No optimization without measurement
- [ ] Paths calculated from known roots (never assume)
- [ ] Setup completed before irreversible operations
- [ ] Preconditions validated early
- [ ] Names considered carefully (5+ alternatives evaluated)
- [ ] Code has clear hierarchy and predictable organization

**Guiding principles**:

- "Does this design actually make sense, or am I just agreeing?"
- "Is this the simplest thing that could possibly work?"
- "If I delete this, can I recreate it from git history?"

---

## Lessons from Real Mistakes

### Case Study: Duplicate Transformation Logic

**The Mistake:**
Added `unix://` → `vsock://` transformation in `litebox.rs` when it already existed in `krun/engine.rs`.

**Why it happened:**

- Didn't search before implementing (violated Rule #3)
- Became "yes man" to own design (violated Rule #0)
- Didn't question which layer should own the logic (violated Rule #7)
- Treated rules as post-commit review, not pre-commit design

**How rules should have prevented it:**

| Rule                             | What Should Have Happened                                   |
|----------------------------------|-------------------------------------------------------------|
| **#0 (Don't Be Yes Man)**        | "Does this already exist?" → Search first                   |
| **#3 (Search Before Implement)** | `grep -r "transform.*vsock" src/` → Found in krun/engine.rs |
| **#5 (DRY)**                     | Check for existing transformation logic                     |
| **#7 (Minimal Knowledge)**       | "Why does litebox know about krun details?" → Wrong layer   |

**What would have caught it:**

```bash
# BEFORE writing transformation code:
$ grep -r "transform.*vsock" boxlite/src/
boxlite/src/engines/krun/engine.rs:113:fn transform_guest_args
# → Found it! Don't duplicate.

# BEFORE adding krun-specific logic to litebox:
$ grep -r "GUEST_AGENT_PORT" boxlite/src/
# → Only in krun/ directory
# → This is a krun detail, doesn't belong in litebox.rs
```

**The Fix:**

```rust
// ❌ WRONG: litebox.rs duplicating krun logic
fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
    let guest_transport = match transport {
        Transport::Unix { .. } => Transport::vsock(2695),  // Duplicate!
        ...
    };
}

// ✅ RIGHT: Pass as-is, let engine handle it
fn create_guest_entrypoint(&self, transport: &Transport) -> GuestEntrypoint {
    let uri = transport.to_uri();  // krun/engine.rs transforms later
    format!("exec boxlite-guest --listen {}", uri)
}
```

**Key Lesson:**
Rules are not a QA checklist to run after coding. They are a **design thinking framework** to apply BEFORE and DURING coding. Always:

1. Search first (`grep`)
2. Read affected files completely
3. Question ownership/layering
4. THEN code

---

## How to Use These Rules

**❌ WRONG: Checklist after coding**

1. Write code
2. Check if it follows rules
3. Fix violations

**✅ RIGHT: Active thinking before coding**

1. Search for existing solutions (`grep -r "pattern" src/`)
2. Read affected files completely (don't skim)
3. Analyze ownership/layering ("Who should know this?")
4. Question necessity ("What breaks if I don't add this?")
5. THEN code (following rules)

**The rules are not a QA checklist—they're a design thinking framework.**
