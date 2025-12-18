//! Platform-specific filesystem operations.
//!
//! This module dispatches to platform-specific implementations:
//! - Linux: Real bind mounts with mount(2) syscall
//! - macOS: Symlink fallback (macOS lacks bind mount support)

use boxlite_shared::errors::BoxliteResult;
use std::path::Path;

use super::BindMountConfig;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

/// Handle to a bind mount that cleans up on drop.
///
/// On Linux, this unmounts the bind mount.
/// On macOS, this removes the symlink.
pub struct BindMountHandle {
    #[cfg(target_os = "linux")]
    inner: linux::LinuxBindMount,
    #[cfg(target_os = "macos")]
    inner: macos::MacosSymlink,
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    _target: PathBuf,
}

impl BindMountHandle {
    /// Get the target path of this bind mount.
    pub fn target(&self) -> &Path {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.inner.target()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            &self._target
        }
    }

    /// Explicitly unmount/cleanup the bind mount.
    ///
    /// This is called automatically on drop, but can be called manually
    /// if you need to handle errors.
    pub fn unmount(self) -> BoxliteResult<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.inner.cleanup()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Ok(())
        }
    }
}

/// Create a bind mount (or platform-equivalent).
///
/// # Platform behavior
///
/// - **Linux**: Creates a real bind mount using mount(2) with MS_BIND.
///   If `read_only` is true, remounts with MS_RDONLY.
///   Uses MS_SLAVE propagation to prevent mount events from propagating.
///
/// - **macOS**: Creates a symbolic link as a fallback.
///   The `read_only` flag has no effect on macOS (symlinks inherit permissions).
///
/// # Example
///
/// ```no_run
/// use boxlite::fs::{create_bind_mount, BindMountConfig};
/// use std::path::Path;
///
/// let source = Path::new("/source/path");
/// let target = Path::new("/target/path");
///
/// let config = BindMountConfig::new(source, target).read_only();
/// let handle = create_bind_mount(&config)?;
///
/// // Mount is automatically cleaned up when handle is dropped
/// # Ok::<(), boxlite_shared::errors::BoxliteError>(())
/// ```
pub fn create_bind_mount(config: &BindMountConfig) -> BoxliteResult<BindMountHandle> {
    #[cfg(target_os = "linux")]
    {
        let inner = linux::LinuxBindMount::create(config)?;
        Ok(BindMountHandle { inner })
    }

    #[cfg(target_os = "macos")]
    {
        let inner = macos::MacosSymlink::create(config)?;
        Ok(BindMountHandle { inner })
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(boxlite_shared::errors::BoxliteError::Unsupported(
            "Bind mounts not supported on this platform".to_string(),
        ))
    }
}
