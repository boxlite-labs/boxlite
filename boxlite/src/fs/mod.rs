//! Filesystem utilities for host-side operations.
//!
//! This module provides cross-platform abstractions for filesystem operations
//! that differ between Linux and macOS, particularly bind mounts.

mod platform;

pub use platform::{BindMountHandle, create_bind_mount};

use std::path::Path;

/// Configuration for creating a bind mount.
#[derive(Debug, Clone)]
pub struct BindMountConfig<'a> {
    /// Source path (the directory to bind from).
    pub source: &'a Path,
    /// Target path (where the bind mount will appear).
    pub target: &'a Path,
    /// Whether the mount should be read-only.
    pub read_only: bool,
}

impl<'a> BindMountConfig<'a> {
    /// Create a new bind mount config.
    pub fn new(source: &'a Path, target: &'a Path) -> Self {
        Self {
            source,
            target,
            read_only: false,
        }
    }

    /// Set the mount as read-only.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}
