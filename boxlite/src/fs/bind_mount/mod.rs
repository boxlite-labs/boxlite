//! Bind mount implementation.
//!
//! Provides bind mount functionality for Linux with automatic strategy selection:
//! - Privileged (CAP_SYS_ADMIN): Native mount(2) syscall
//! - Rootless: FUSE passthrough filesystem

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

// Re-exports
pub use config::BindMountConfig;
pub use handle::BindMountHandle;

mod config;
mod handle;

#[cfg(target_os = "linux")]
mod native;

#[cfg(target_os = "linux")]
mod fuse;

/// Create a bind mount with automatic strategy selection.
///
/// Selects strategy based on available capabilities:
/// - With CAP_SYS_ADMIN: Native mount(2) syscall
/// - Without CAP_SYS_ADMIN: FUSE passthrough filesystem
#[cfg(target_os = "linux")]
pub fn create_bind_mount(config: &BindMountConfig) -> BoxliteResult<BindMountHandle> {
    validate_config(config)?;
    create_bind_mount_linux(config)
}

#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub fn create_bind_mount(_config: &BindMountConfig) -> BoxliteResult<BindMountHandle> {
    Err(BoxliteError::Unsupported(
        "Bind mounts are only supported on Linux".to_string(),
    ))
}

#[cfg(target_os = "linux")]
fn validate_config(config: &BindMountConfig) -> BoxliteResult<()> {
    if !config.source.exists() {
        return Err(BoxliteError::Storage(format!(
            "Bind mount source does not exist: {}",
            config.source.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_target_dir_exists(target: &std::path::Path) -> BoxliteResult<()> {
    std::fs::create_dir_all(target).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create bind mount target directory {}: {}",
            target.display(),
            e
        ))
    })
}

#[cfg(target_os = "linux")]
fn create_bind_mount_linux(config: &BindMountConfig) -> BoxliteResult<BindMountHandle> {
    use fuse::FuseBindMount;
    use native::NativeBindMount;

    if has_cap_sys_admin() {
        tracing::debug!("Using native bind mount (CAP_SYS_ADMIN available)");
        let inner = NativeBindMount::create(config)?;
        Ok(BindMountHandle::new(Box::new(inner)))
    } else {
        tracing::debug!("Using FUSE bind mount (rootless mode)");
        let inner = FuseBindMount::create(config)?;
        Ok(BindMountHandle::new(Box::new(inner)))
    }
}

#[cfg(target_os = "linux")]
fn has_cap_sys_admin() -> bool {
    caps::has_cap(
        None,
        caps::CapSet::Effective,
        caps::Capability::CAP_SYS_ADMIN,
    )
    .unwrap_or(false)
}

// ============================================================================
// Trait definition (Linux only)
// ============================================================================

/// Trait for bind mount implementations.
#[cfg(target_os = "linux")]
pub(crate) trait BindMountImpl: Send + Sync {
    fn target(&self) -> &std::path::Path;
    fn unmount(&mut self) -> BoxliteResult<()>;
}
