//! OCI images blob storage operations.
//!
//! This module provides low-level storage operations for OCI images artifacts:
//! manifests, layers, and config blobs. It handles file I/O, path management,
//! and integrity verification.
//!
//! Does NOT handle:
//! - Image metadata/indexing (ImageIndex's responsibility)
//! - Registry communication (ImageManager's responsibility)
//! - Cache lookup logic (ImageManager's responsibility)

use std::path::{Path, PathBuf};

use oci_client::manifest::OciManifest;

use super::manager::LayerInfo;
use crate::images::archive;
use crate::runtime::layout::ImageFilesystemLayout;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

// ============================================================================
// IMAGE STORE
// ============================================================================

/// Manages persistent storage of OCI images blobs.
///
/// Provides low-level operations for storing and loading images artifacts
/// (manifests, layers, configs) with digest-based naming and integrity
/// verification.
pub struct ImageStorage {
    layout: ImageFilesystemLayout,
}

impl std::fmt::Debug for ImageStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageStorage")
            .field("images_dir", &self.layout.root())
            .finish()
    }
}

impl ImageStorage {
    /// Create new images store for the given images directory
    pub fn new(images_dir: PathBuf) -> BoxliteResult<Self> {
        let layout = ImageFilesystemLayout::new(images_dir);
        layout.prepare()?;
        Ok(Self { layout })
    }

    // ========================================================================
    // MANIFEST OPERATIONS [atomic, &self]
    // ========================================================================

    /// Save manifest to disk using digest as filename.
    ///
    /// **Mutability**: Atomic - writes file only if it doesn't exist, safe for
    /// concurrent access (idempotent check-then-write).
    pub fn save_manifest(&self, manifest: &OciManifest, digest: &str) -> BoxliteResult<()> {
        let manifest_path = self.manifest_path(digest);

        if manifest_path.exists() {
            tracing::debug!("Manifest already exists: {}", digest);
            return Ok(());
        }

        let manifest_json = serde_json::to_string_pretty(manifest)
            .map_err(|e| BoxliteError::Storage(format!("Failed to serialize manifest: {}", e)))?;

        std::fs::write(&manifest_path, manifest_json).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to write manifest to {}: {}",
                manifest_path.display(),
                e
            ))
        })?;

        tracing::debug!("Saved manifest: {}", digest);
        Ok(())
    }

    /// Load manifest from disk by digest.
    ///
    /// **Mutability**: Immutable - reads file only, no state changes.
    pub fn load_manifest(&self, digest: &str) -> BoxliteResult<OciManifest> {
        let manifest_path = self.manifest_path(digest);

        if !manifest_path.exists() {
            return Err(BoxliteError::Storage(format!(
                "Manifest not found: {}",
                digest
            )));
        }

        let manifest_json = std::fs::read_to_string(&manifest_path).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to read manifest {}: {}",
                manifest_path.display(),
                e
            ))
        })?;

        let manifest: OciManifest = serde_json::from_str(&manifest_json)
            .map_err(|e| BoxliteError::Storage(format!("Failed to parse manifest: {}", e)))?;

        Ok(manifest)
    }

    /// Check if manifest exists on disk.
    ///
    /// **Mutability**: Immutable - reads filesystem only, no state changes.
    pub fn has_manifest(&self, digest: &str) -> bool {
        self.manifest_path(digest).exists()
    }

    /// Get path to manifest file.
    ///
    /// **Mutability**: Immutable - pure path computation, no I/O.
    pub fn manifest_path(&self, digest: &str) -> PathBuf {
        let filename = digest.replace(':', "-");
        self.layout
            .manifests_dir()
            .join(format!("{}.json", filename))
    }

    // ========================================================================
    // LAYER OPERATIONS [mixed mutability]
    // ========================================================================

    /// Check if layer tarball exists on disk.
    ///
    /// **Mutability**: Immutable - reads filesystem only, no state changes.
    pub fn has_layer(&self, digest: &str) -> bool {
        self.layer_tarball_path(digest).exists()
    }

    /// Verify layer integrity by computing SHA256 hash and comparing.
    ///
    /// **Mutability**: Immutable - reads file and computes hash, no state changes.
    pub async fn verify_layer(&self, digest: &str) -> BoxliteResult<bool> {
        use sha2::{Digest, Sha256};

        let layer_path = self.layer_tarball_path(digest);

        if !layer_path.exists() {
            return Ok(false);
        }

        // Read file and compute hash
        let file_data = tokio::fs::read(&layer_path).await.map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to read layer {} for verification: {}",
                layer_path.display(),
                e
            ))
        })?;

        let mut hasher = Sha256::new();
        hasher.update(&file_data);
        let computed_hash = format!("sha256:{:x}", hasher.finalize());

        if computed_hash != digest {
            tracing::error!(
                "Layer integrity check failed:\n  Expected: {}\n  Computed: {}\n  File size: {} bytes",
                digest,
                computed_hash,
                file_data.len()
            );
            return Ok(false);
        }

        Ok(true)
    }

    /// Get path to layer tarball.
    ///
    /// **Mutability**: Immutable - pure path computation, no I/O.
    pub fn layer_tarball_path(&self, digest: &str) -> PathBuf {
        let filename = digest.replace(':', "-");
        self.layout
            .layers_dir()
            .join(format!("{}.tar.gz", filename))
    }

    /// Get path to extracted layer directory.
    ///
    /// **Mutability**: Immutable - pure path computation, no I/O.
    pub fn layer_extracted_path(&self, digest: &str) -> PathBuf {
        let filename = digest.replace(':', "-");
        self.layout.extracted_dir().join(filename)
    }

    /// Extract layer tarball to cache directory (keeping whiteout markers).
    ///
    /// **Mutability**: Atomic - uses temp directory + atomic rename pattern.
    /// Safe for concurrent access; only one thread wins, losers clean up.
    ///
    /// CRITICAL: This extracts the layer but does NOT process whiteouts.
    /// Whiteout markers (.wh.* files) are kept in the cached layer because:
    /// - They indicate files to delete from LOWER layers
    /// - Processing them on individual layers would lose deletion information
    /// - Whiteouts are processed INLINE when copying layers (not after merge)
    ///
    /// Example:
    /// - layer0 has: /bin/sh, /bin/bash
    /// - layer1 has: /bin/.wh.sh (delete sh), /bin/newfile
    /// - If we process whiteouts on layer1 alone, .wh.sh is removed but sh isn't deleted
    /// - When copying layer1 on top of layer0: .wh.sh triggers deletion of sh
    /// - Correct: keep .wh.sh in cached layer1, process during copy operation
    pub fn extract_layer(&self, digest: &str, tarball_path: &Path) -> BoxliteResult<()> {
        let extracted_path = self.layer_extracted_path(digest);

        // Fast path: already extracted
        if extracted_path.exists() {
            tracing::trace!("Layer {} already extracted (cached)", digest);
            return Ok(());
        }

        // Extract to a unique temp directory to avoid race conditions
        // Use PID + random UUID to ensure uniqueness across threads and processes
        let temp_suffix = format!("{}.extracting", uuid::Uuid::new_v4().simple());
        let temp_path = extracted_path.with_extension(temp_suffix);

        // Create temp directory
        std::fs::create_dir_all(&temp_path).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create temp extraction directory {}: {}",
                temp_path.display(),
                e
            ))
        })?;

        // Extract tarball to temp directory - keep .wh.* files!
        if let Err(e) = archive::extract_layer_tarball_streaming(tarball_path, &temp_path) {
            // Clean up temp dir on extraction failure
            let _ = std::fs::remove_dir_all(&temp_path);
            return Err(e);
        }

        // Atomic rename: only one thread/process wins
        match std::fs::rename(&temp_path, &extracted_path) {
            Ok(()) => {
                tracing::debug!(
                    "Extracted layer {} (with whiteout markers) to {}",
                    digest,
                    extracted_path.display()
                );
            }
            Err(e) => {
                // Another thread/process won the race - clean up our temp dir
                let _ = std::fs::remove_dir_all(&temp_path);

                // Check if the winner succeeded (directory exists)
                if extracted_path.exists() {
                    tracing::debug!(
                        "Layer {} already extracted by another thread/process",
                        digest
                    );
                } else {
                    // Neither we nor the winner succeeded - this is an error
                    return Err(BoxliteError::Storage(format!(
                        "Failed to rename temp directory to {}: {} (and no other extraction succeeded)",
                        extracted_path.display(),
                        e
                    )));
                }
            }
        }

        Ok(())
    }

    /// Start a staged download for a layer blob.
    ///
    /// **Mutability**: Atomic - creates unique temp file with random suffix.
    /// Safe for concurrent access; each caller gets its own temp file.
    ///
    /// Returns a StagedDownload handle that manages the temp file lifecycle.
    /// Use `staged.file()` to get the file for writing.
    pub async fn stage_layer_download(&self, digest: &str) -> BoxliteResult<StagedDownload> {
        // Extract expected hash from digest
        let expected_hash = digest
            .strip_prefix("sha256:")
            .ok_or_else(|| BoxliteError::Storage("Invalid digest format, expected sha256:".into()))?
            .to_string();

        // Generate random suffix to prevent collision in parallel downloads
        let random_suffix = uuid::Uuid::new_v4().simple();
        let filename = digest.replace(':', "-");
        let staged_path = self
            .layout
            .layers_dir()
            .join(format!("{}.{}.downloading", filename, random_suffix));

        let file = tokio::fs::File::create(&staged_path).await.map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create temp file {}: {}",
                staged_path.display(),
                e
            ))
        })?;

        Ok(StagedDownload::new(
            staged_path,
            self.layer_tarball_path(digest),
            expected_hash,
            file,
        ))
    }

    // ========================================================================
    // CONFIG OPERATIONS [mixed mutability]
    // ========================================================================

    /// Check if config blob exists on disk.
    ///
    /// **Mutability**: Immutable - reads filesystem only, no state changes.
    pub fn has_config(&self, digest: &str) -> bool {
        self.config_path(digest).exists()
    }

    /// Load config blob from disk.
    ///
    /// **Mutability**: Immutable - reads file only, no state changes.
    pub fn load_config(&self, digest: &str) -> BoxliteResult<String> {
        let config_path = self.config_path(digest);

        if !config_path.exists() {
            return Err(BoxliteError::Storage(format!(
                "Config blob not found: {}. Did you call pull() first?",
                digest
            )));
        }

        std::fs::read_to_string(&config_path).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to read config {}: {}",
                config_path.display(),
                e
            ))
        })
    }

    /// Get path to config blob.
    ///
    /// **Mutability**: Immutable - pure path computation, no I/O.
    pub fn config_path(&self, digest: &str) -> PathBuf {
        // Config blobs stored in configs directory
        self.layout
            .configs_dir()
            .join(format!("{}.json", digest.replace(':', "-")))
    }

    /// Create file for writing config blob.
    ///
    /// **Mutability**: Atomic - creates file at content-addressed path.
    /// Safe for concurrent access; same digest always writes to same path.
    ///
    /// Ensures parent directory exists and returns an open file handle.
    pub async fn create_config_file(&self, digest: &str) -> BoxliteResult<tokio::fs::File> {
        let config_path = self.config_path(digest);

        // Ensure parent directory exists
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to create config directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        // Create file for writing
        tokio::fs::File::create(&config_path).await.map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to create config file {}: {}",
                config_path.display(),
                e
            ))
        })
    }

    // ========================================================================
    // UTILITY OPERATIONS [immutable, &self]
    // ========================================================================

    /// Find manifest digest by scanning manifests directory for matching layers.
    ///
    /// **Mutability**: Immutable - reads and scans files only, no state changes.
    ///
    /// This is used after pulling to locate which cached manifest corresponds
    /// to the pulled layers.
    pub fn find_manifest_for_layers(&self, layers: &[LayerInfo]) -> BoxliteResult<String> {
        let manifests_dir = self.layout.manifests_dir();

        // List all manifest files
        let entries = std::fs::read_dir(&manifests_dir).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to read manifests directory {}: {}",
                manifests_dir.display(),
                e
            ))
        })?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }

            // Read and parse manifest
            if let Ok(content) = std::fs::read_to_string(&path)
                && let Ok(manifest) = serde_json::from_str::<OciManifest>(&content)
            {
                // Check if this manifest has matching layers
                let manifest_layers = match manifest {
                    OciManifest::Image(img) => img.layers,
                    _ => continue,
                };

                // Compare layer digests
                if manifest_layers.len() == layers.len()
                    && manifest_layers
                        .iter()
                        .zip(layers.iter())
                        .all(|(m, l)| m.digest == l.digest)
                {
                    // Found matching manifest - extract digest from filename
                    let filename = path.file_stem().unwrap().to_str().unwrap();
                    let digest = filename.replace('-', ":");
                    return Ok(digest);
                }
            }
        }

        Err(BoxliteError::Storage(
            "Could not find manifest digest for layers".into(),
        ))
    }

    /// Verify all blobs for given layer digests exist on disk.
    ///
    /// **Mutability**: Immutable - reads filesystem only, no state changes.
    pub fn verify_blobs_exist(&self, layer_digests: &[String]) -> bool {
        layer_digests.iter().all(|digest| self.has_layer(digest))
    }

    /// Get the images directory path.
    ///
    /// **Mutability**: Immutable - returns reference to stored path.
    ///
    /// This is used by ImageIndex for saving/loading the index file.
    pub fn images_dir(&self) -> &Path {
        self.layout.root()
    }

    /// Get the layers directory path.
    ///
    /// **Mutability**: Immutable - returns path to layers directory.
    #[allow(unused)]
    pub fn layer_dir(&self) -> PathBuf {
        self.layout.layers_dir()
    }

    /// Get path to disk image for an image digest with specific format.
    ///
    /// Disk images are stored in `{images_dir}/disk-images/{digest}.{ext}`.
    /// This returns the path regardless of whether the file exists.
    pub fn disk_image_path(
        &self,
        image_digest: &str,
        format: crate::volumes::DiskFormat,
    ) -> PathBuf {
        let filename = image_digest.replace(':', "-");
        self.layout
            .disk_images_dir()
            .join(format!("{}.{}", filename, format.as_str()))
    }

    /// Find existing disk image for an image digest, checking all known formats.
    ///
    /// Returns the path and format if a cached disk image exists.
    pub fn find_disk_image(
        &self,
        image_digest: &str,
    ) -> Option<(PathBuf, crate::volumes::DiskFormat)> {
        use crate::volumes::DiskFormat;

        // Check for ext4 format first (most common for rootfs)
        let ext4_path = self.disk_image_path(image_digest, DiskFormat::Ext4);
        if ext4_path.exists() {
            return Some((ext4_path, DiskFormat::Ext4));
        }

        // Check for qcow2 format
        let qcow2_path = self.disk_image_path(image_digest, DiskFormat::Qcow2);
        if qcow2_path.exists() {
            return Some((qcow2_path, DiskFormat::Qcow2));
        }

        None
    }
}

// ============================================================================
// STAGED DOWNLOAD
// ============================================================================

/// Handle for an in-progress download with atomic commit semantics
///
/// Downloads to a temp file first, then verifies integrity and atomically
/// moves to the final location. Temp file uses random suffix to prevent
/// collision in parallel downloads.
///
/// # Example
/// ```ignore
/// let mut staged = store.stage_layer_download(digest).await?;
/// // Write data to file...
/// client.pull_blob(reference, descriptor, staged.file()).await?;
/// if staged.commit().await? {
///     println!("Download verified and committed");
/// } else {
///     println!("Verification failed, temp file cleaned up");
/// }
/// ```
pub struct StagedDownload {
    staged_path: PathBuf,
    final_path: PathBuf,
    expected_hash: String,
    file: Option<tokio::fs::File>,
}

impl StagedDownload {
    /// Create a new staged download
    fn new(
        staged_path: PathBuf,
        final_path: PathBuf,
        expected_hash: String,
        file: tokio::fs::File,
    ) -> Self {
        Self {
            staged_path,
            final_path,
            expected_hash,
            file: Some(file),
        }
    }

    /// Get mutable reference to the file for writing
    pub fn file(&mut self) -> &mut tokio::fs::File {
        self.file.as_mut().expect("file already consumed")
    }

    /// Get the staged file path (for debugging/logging)
    #[allow(unused)]
    pub fn staged_path(&self) -> &Path {
        &self.staged_path
    }

    #[allow(unused)]
    pub fn final_path(&self) -> &Path {
        &self.final_path
    }

    /// Verify integrity and atomically move to final location
    ///
    /// Returns Ok(true) if verification passed and file was committed,
    /// Ok(false) if verification failed (temp file is cleaned up).
    /// Consumes self to prevent further use after commit.
    pub async fn commit(mut self) -> BoxliteResult<bool> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncReadExt;

        // Drop the write handle before reading
        self.file.take();

        if !self.staged_path.exists() {
            return Err(BoxliteError::Storage(format!(
                "Temp file not found: {}",
                self.staged_path.display()
            )));
        }

        // Verify integrity
        let mut file = tokio::fs::File::open(&self.staged_path)
            .await
            .map_err(|e| BoxliteError::Storage(format!("Failed to open temp file: {}", e)))?;

        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = file
                .read(&mut buffer)
                .await
                .map_err(|e| BoxliteError::Storage(format!("Failed to read temp file: {}", e)))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        let computed_hash = format!("{:x}", hasher.finalize());

        if computed_hash != self.expected_hash {
            // Verification failed - clean up temp file
            let _ = tokio::fs::remove_file(&self.staged_path).await;
            return Ok(false);
        }

        // Atomically move temp file to final location
        tokio::fs::rename(&self.staged_path, &self.final_path)
            .await
            .map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to move {} to {}: {}",
                    self.staged_path.display(),
                    self.final_path.display(),
                    e
                ))
            })?;

        Ok(true)
    }

    /// Clean up the temp file without committing
    ///
    /// Call this on download failure or cancellation.
    pub async fn abort(mut self) {
        self.file.take();
        let _ = tokio::fs::remove_file(&self.staged_path).await;
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_new_creates_directories() {
        let temp_dir = tempfile::tempdir().unwrap();
        let images_dir = temp_dir.path().join("images");

        let store = ImageStorage::new(images_dir.clone()).unwrap();

        assert!(images_dir.join("manifests").exists());
        assert!(images_dir.join("layers").exists());
        assert_eq!(store.images_dir(), images_dir);
    }

    #[test]
    fn test_manifest_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let path = store.manifest_path("sha256:abc123");
        assert_eq!(path, temp_dir.path().join("manifests/sha256-abc123.json"));
    }

    #[test]
    fn test_layer_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let path = store.layer_tarball_path("sha256:layer1");
        assert_eq!(path, temp_dir.path().join("layers/sha256-layer1.tar.gz"));
    }

    #[test]
    fn test_config_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let path = store.config_path("sha256:config1");
        assert_eq!(path, temp_dir.path().join("configs/sha256-config1.json"));
    }

    #[test]
    fn test_has_manifest() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        assert!(!store.has_manifest("sha256:abc123"));

        // Create a manifest file
        let manifest_path = store.manifest_path("sha256:abc123");
        std::fs::write(manifest_path, "{}").unwrap();

        assert!(store.has_manifest("sha256:abc123"));
    }

    #[test]
    fn test_has_layer() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        assert!(!store.has_layer("sha256:layer1"));

        // Create a layer file
        let layer_path = store.layer_tarball_path("sha256:layer1");
        std::fs::write(layer_path, b"fake layer data").unwrap();

        assert!(store.has_layer("sha256:layer1"));
    }

    #[test]
    fn test_has_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        assert!(!store.has_config("sha256:config1"));

        // Create a config file
        let config_path = store.config_path("sha256:config1");
        std::fs::write(config_path, "{}").unwrap();

        assert!(store.has_config("sha256:config1"));
    }

    #[test]
    fn test_load_config() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let config_path = store.config_path("sha256:config1");
        std::fs::write(&config_path, r#"{"foo": "bar"}"#).unwrap();

        let config = store.load_config("sha256:config1").unwrap();
        assert_eq!(config, r#"{"foo": "bar"}"#);
    }

    #[test]
    fn test_verify_blobs_exist() {
        let temp_dir = tempfile::tempdir().unwrap();
        let store = ImageStorage::new(temp_dir.path().to_path_buf()).unwrap();

        let layer1 = "sha256:layer1".to_string();
        let layer2 = "sha256:layer2".to_string();

        // No layers exist yet
        assert!(!store.verify_blobs_exist(&[layer1.clone(), layer2.clone()]));

        // Create first layer
        std::fs::write(store.layer_tarball_path(&layer1), b"data1").unwrap();
        assert!(!store.verify_blobs_exist(&[layer1.clone(), layer2.clone()]));

        // Create second layer
        std::fs::write(store.layer_tarball_path(&layer2), b"data2").unwrap();
        assert!(store.verify_blobs_exist(&[layer1, layer2]));
    }
}
