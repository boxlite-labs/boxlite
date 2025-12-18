//! Image index for tracking locally cached OCI images.
//!
//! The index maps images references to their cached metadata, enabling
//! offline operation by checking local availability before hitting the registry.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

// ============================================================================
// PUBLIC TYPES
// ============================================================================

/// Index of locally cached images.
///
/// Maps images references (e.g., "docker.io/library/python:alpine") to
/// their cached metadata. Serialized as JSON for human readability.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ImageIndex {
    /// Schema version for future compatibility
    pub version: String,

    /// Map of images reference → cached images metadata
    pub images: HashMap<String, CachedImage>,
}

/// Metadata for a cached images.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CachedImage {
    /// Manifest digest of the final image (sha256:...)
    /// For multi-platform images, this is the platform-specific manifest digest
    pub manifest_digest: String,

    /// Config blob digest (sha256:...)
    /// Empty string for legacy entries (will trigger re-pull)
    #[serde(default)]
    pub config_digest: String,

    /// Layer digests in order
    pub layers: Vec<String>,

    /// When the images was cached (ISO 8601)
    pub cached_at: String,

    /// Whether all layers are fully downloaded
    pub complete: bool,
}

// ============================================================================
// IMPLEMENTATION
// ============================================================================

impl ImageIndex {
    /// Create a new empty index
    pub fn new() -> Self {
        Self {
            version: "1.0".to_string(),
            images: HashMap::new(),
        }
    }

    /// Load index from disk
    ///
    /// Returns empty index if file doesn't exist or is corrupted.
    pub fn load(images_dir: &Path) -> BoxliteResult<Self> {
        let index_path = images_dir.join("index.json");

        if !index_path.exists() {
            tracing::debug!("Index file not found, creating new index");
            return Ok(Self::new());
        }

        match std::fs::read_to_string(&index_path) {
            Ok(contents) => match serde_json::from_str::<Self>(&contents) {
                Ok(index) => {
                    tracing::debug!("Loaded index with {} images", index.len());
                    Ok(index)
                }
                Err(e) => {
                    tracing::warn!("Corrupted index file, creating new: {}", e);
                    Ok(Self::new())
                }
            },
            Err(e) => {
                tracing::warn!("Failed to read index file, creating new: {}", e);
                Ok(Self::new())
            }
        }
    }

    /// Save index to disk
    pub fn save(&self, images_dir: &Path) -> BoxliteResult<()> {
        let index_path = images_dir.join("index.json");

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| BoxliteError::Storage(format!("failed to serialize index: {e}")))?;

        std::fs::write(&index_path, json)
            .map_err(|e| BoxliteError::Storage(format!("failed to write index: {e}")))?;

        tracing::debug!("Saved index with {} images", self.images.len());
        Ok(())
    }

    /// Get cached images by reference
    ///
    /// Returns None if images not in index.
    pub fn get(&self, reference: &str) -> Option<&CachedImage> {
        self.images.get(reference)
    }

    /// Add or update cached images
    pub fn upsert(&mut self, reference: String, image: CachedImage) {
        self.images.insert(reference, image);
    }

    /// Remove cached images from index
    #[allow(dead_code)]
    pub fn remove(&mut self, reference: &str) -> Option<CachedImage> {
        self.images.remove(reference)
    }

    /// Get number of cached images in index
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Check if index is empty
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_new() {
        let index = ImageIndex::new();
        assert_eq!(index.version, "1.0");
        assert_eq!(index.images.len(), 0);
    }

    #[test]
    fn test_index_upsert() {
        let mut index = ImageIndex::new();

        let image = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec!["sha256:layer1".to_string()],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        index.upsert("python:alpine".to_string(), image.clone());
        assert_eq!(index.images.len(), 1);

        // Update same images
        let updated = CachedImage {
            complete: false,
            ..image
        };
        index.upsert("python:alpine".to_string(), updated);
        assert_eq!(index.images.len(), 1);
        assert!(!index.get("python:alpine").unwrap().complete);
    }

    #[test]
    fn test_index_get() {
        let mut index = ImageIndex::new();

        let image = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec![],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        index.upsert("python:alpine".to_string(), image);

        assert!(index.get("python:alpine").is_some());
        assert!(index.get("ubuntu:22.04").is_none());
    }

    #[test]
    fn test_index_remove() {
        let mut index = ImageIndex::new();

        let image = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec![],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        index.upsert("python:alpine".to_string(), image);
        assert_eq!(index.images.len(), 1);

        let removed = index.remove("python:alpine");
        assert!(removed.is_some());
        assert_eq!(index.images.len(), 0);
    }

    #[test]
    fn test_index_save_load() {
        let temp_dir = tempfile::tempdir().unwrap();
        let images_dir = temp_dir.path();

        let mut index = ImageIndex::new();
        let image = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec!["sha256:layer1".to_string(), "sha256:layer2".to_string()],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        index.upsert("python:alpine".to_string(), image);

        // Save
        index.save(images_dir).unwrap();

        // Load
        let loaded = ImageIndex::load(images_dir).unwrap();
        assert_eq!(loaded.images.len(), 1);

        let loaded_image = loaded.get("python:alpine").unwrap();
        assert_eq!(loaded_image.manifest_digest, "sha256:abc123");
        assert_eq!(loaded_image.layers.len(), 2);
        assert!(loaded_image.complete);
    }

    #[test]
    fn test_index_load_missing_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let images_dir = temp_dir.path();

        let index = ImageIndex::load(images_dir).unwrap();
        assert_eq!(index.images.len(), 0);
    }

    #[test]
    fn test_index_load_corrupted_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let images_dir = temp_dir.path();

        // Write invalid JSON
        std::fs::write(images_dir.join("index.json"), "invalid json").unwrap();

        // Should return empty index, not error
        let index = ImageIndex::load(images_dir).unwrap();
        assert_eq!(index.images.len(), 0);
    }
}
