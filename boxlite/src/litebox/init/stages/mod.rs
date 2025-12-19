//! Initialization stages.
//!
//! Each stage is a function with typed input/output.
//! Stages do ONE thing and have no side effects beyond their output.
//!
//! ## Stage Dependency Graph
//!
//! ```text
//! Filesystem ─────┐
//!                 │
//! Rootfs ─────────┼──→ Config ──→ Spawn ──→ Guest
//!                 │
//! GuestRootfs ────┘
//!
//! Parallel:   [Filesystem, Rootfs, GuestRootfs]
//! Sequential: Config → Spawn → Guest
//! ```

pub mod container_rootfs;
pub mod filesystem;
pub mod guest_init;
pub mod guest_rootfs;
pub mod spawn;
pub mod vmm_config;
