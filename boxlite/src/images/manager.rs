//! OCI images management: pulling, caching, and manifest handling.
//!
//! This module provides:
//! - `ImageManager`: Public facade for image operations
//! - `ImageManifest`, `LayerInfo`: Internal types for manifest data
//!
//! Architecture:
//! - `ImageManager` holds `Arc<ImageStore>` (thread-safe store)
//! - `ImageStore` handles all locking internally
//! - `ImageObject` also holds `Arc<ImageStore>` for layer access

use std::path::PathBuf;
use std::sync::Arc;

use super::object::ImageObject;
use crate::images::store::{ImageStore, SharedImageStore};
use boxlite_shared::errors::BoxliteResult;

// ============================================================================
// INTERNAL TYPES
// ============================================================================

#[derive(Debug, Clone)]
pub(super) struct ImageManifest {
    /// Manifest digest of the final image (platform-specific for multi-platform images)
    pub(super) manifest_digest: String,
    pub(super) layers: Vec<LayerInfo>,
    pub(super) config_digest: String,
}

#[derive(Debug, Clone)]
pub(super) struct LayerInfo {
    pub(super) digest: String,
    pub(super) media_type: String,
}

// ============================================================================
// IMAGE MANAGER (Public Facade)
// ============================================================================

/// Public API for OCI image operations.
///
/// This is a lightweight facade over `Arc<ImageStore>`. It can be cloned
/// cheaply and all clones share the same underlying store.
///
/// Thread Safety: `ImageStore` handles all locking internally. Multiple
/// concurrent pulls are safe and will share downloaded layers.
///
/// # Example
///
/// ```no_run
/// use boxlite::images::ImageManager;
/// use std::path::PathBuf;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let manager = ImageManager::new(PathBuf::from("/tmp/images"))?;
///
/// // Pull an image
/// let image = manager.pull("python:alpine").await?;
///
/// // Access image information
/// println!("Image: {}", image.reference());
/// println!("Layers: {}", image.layer_count());
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ImageManager {
    store: SharedImageStore,
}

impl std::fmt::Debug for ImageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageManager").finish()
    }
}

impl ImageManager {
    /// Create a new image manager for the given images directory.
    pub fn new(images_dir: PathBuf) -> BoxliteResult<Self> {
        let store = Arc::new(ImageStore::new(images_dir)?);
        Ok(Self { store })
    }

    /// Pull an OCI image from a registry.
    ///
    /// Checks local cache first. If the image is already cached and complete,
    /// returns immediately without network access. Otherwise pulls from registry.
    ///
    /// Thread Safety: `ImageStore` handles locking internally. Multiple
    /// concurrent pulls of the same image will only download once.
    pub async fn pull(&self, image_ref: &str) -> BoxliteResult<ImageObject> {
        let manifest = self.store.pull(image_ref).await?;

        Ok(ImageObject::new(
            image_ref.to_string(),
            manifest,
            Arc::clone(&self.store),
        ))
    }
}
