//! Filesystem utilities for host-side operations.

mod bind_mount;

#[cfg(target_os = "linux")]
pub use bind_mount::{BindMountConfig, BindMountHandle, create_bind_mount};
