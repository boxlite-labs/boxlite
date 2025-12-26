//! Bind mount configuration.

use std::path::Path;

/// Configuration for creating a bind mount.
#[derive(Debug, Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct BindMountConfig<'a> {
    pub source: &'a Path,
    pub target: &'a Path,
    pub read_only: bool,
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl<'a> BindMountConfig<'a> {
    pub fn new(source: &'a Path, target: &'a Path) -> Self {
        Self {
            source,
            target,
            read_only: false,
        }
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }
}
