//! Named-volume operations handle.
//!
//! Provides [`VolumeHandle`] for managing named volumes (create, list, get,
//! remove). This mirrors [`ImageHandle`](crate::runtime::ImageHandle): volume
//! management is a distinct capability, surfaced via `BoxliteRuntime::volumes()`
//! and backed by either a local runtime or a REST runtime.
//!
//! The trait is `#[async_trait]` like the other capability backends
//! ([`ImageBackend`](crate::runtime::images::ImageBackend),
//! [`AuthBackend`](crate::runtime::auth::AuthBackend)) so REST backends can
//! perform network calls. `LocalRuntime` backs it with `LocalNamedVolumeStore`;
//! the REST runtime forwards to `/v1/volumes`.

use std::sync::Arc;

use async_trait::async_trait;

use crate::BoxliteResult;
use crate::volumes::VolumeInfo;

/// Internal trait for named-volume management.
///
/// Implemented by `LocalRuntime` (over `LocalNamedVolumeStore`) and the REST
/// runtime (over `/v1/volumes`). A volume is addressed by id or by name.
#[async_trait]
pub(crate) trait VolumeBackend: Send + Sync {
    /// Create a volume, returning its server-assigned metadata (including id).
    /// `name` is optional; the server names the volume after its id without one.
    async fn create_volume(&self, name: Option<&str>) -> BoxliteResult<VolumeInfo>;

    /// List all volumes.
    async fn list_volumes(&self) -> BoxliteResult<Vec<VolumeInfo>>;

    /// Get metadata for a single volume by id or name.
    async fn get_volume(&self, id: &str) -> BoxliteResult<VolumeInfo>;

    /// Remove a volume by id or name. `force` makes a missing volume a no-op.
    async fn remove_volume(&self, id: &str, force: bool) -> BoxliteResult<()>;
}

/// Handle for performing named-volume operations.
///
/// Obtained via [`BoxliteRuntime::volumes()`](crate::BoxliteRuntime::volumes).
#[derive(Clone)]
pub struct VolumeHandle {
    backend: Arc<dyn VolumeBackend>,
}

impl VolumeHandle {
    /// Internal constructor used by `BoxliteRuntime`.
    pub(crate) fn new(backend: Arc<dyn VolumeBackend>) -> Self {
        Self { backend }
    }

    /// Create a volume, returning its metadata (including the assigned id).
    ///
    /// A named volume can be mounted by that name instead of its id. Without
    /// a name the server uses the id, so the volume is still mountable.
    pub async fn create(&self, name: Option<&str>) -> BoxliteResult<VolumeInfo> {
        self.backend.create_volume(name).await
    }

    /// List all volumes.
    pub async fn list(&self) -> BoxliteResult<Vec<VolumeInfo>> {
        self.backend.list_volumes().await
    }

    /// Get metadata for a single volume by id or name.
    pub async fn get(&self, id: &str) -> BoxliteResult<VolumeInfo> {
        self.backend.get_volume(id).await
    }

    /// Remove a volume by id or name. With `force`, a missing volume is a no-op.
    pub async fn remove(&self, id: &str, force: bool) -> BoxliteResult<()> {
        self.backend.remove_volume(id, force).await
    }
}
