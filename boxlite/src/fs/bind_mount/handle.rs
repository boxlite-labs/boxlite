//! Bind mount handle with RAII cleanup.

use boxlite_shared::errors::BoxliteResult;
use std::path::Path;

#[cfg(target_os = "linux")]
use super::BindMountImpl;

/// Handle to a bind mount that cleans up on drop.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct BindMountHandle {
    #[cfg(target_os = "linux")]
    inner: Box<dyn BindMountImpl>,
    #[cfg(not(target_os = "linux"))]
    _marker: std::marker::PhantomData<()>,
}

#[allow(dead_code)]
impl BindMountHandle {
    #[cfg(target_os = "linux")]
    pub(super) fn new(inner: Box<dyn BindMountImpl>) -> Self {
        Self { inner }
    }

    pub fn target(&self) -> &Path {
        #[cfg(target_os = "linux")]
        {
            self.inner.target()
        }
        #[cfg(not(target_os = "linux"))]
        {
            unreachable!("BindMountHandle cannot be created on non-Linux platforms")
        }
    }

    /// Explicitly unmount. Called automatically on drop.
    pub fn unmount(mut self) -> BoxliteResult<()> {
        self.do_unmount()
    }

    #[cfg(target_os = "linux")]
    fn do_unmount(&mut self) -> BoxliteResult<()> {
        self.inner.unmount()
    }

    #[cfg(not(target_os = "linux"))]
    fn do_unmount(&mut self) -> BoxliteResult<()> {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for BindMountHandle {
    fn drop(&mut self) {
        if let Err(e) = self.do_unmount() {
            tracing::warn!(error = %e, "Failed to unmount bind mount on drop");
        }
    }
}
