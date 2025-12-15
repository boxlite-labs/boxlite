use crate::runtime::constants::dirs as const_dirs;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use std::path::{Path, PathBuf};

// ============================================================================
// FILESYSTEM LAYOUT (home directory)
// ============================================================================

#[derive(Clone, Debug)]
pub struct FilesystemLayout {
    home_dir: PathBuf,
}

impl FilesystemLayout {
    pub fn new(home_dir: PathBuf) -> Self {
        Self { home_dir }
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub fn images_dir(&self) -> PathBuf {
        self.home_dir.join(const_dirs::IMAGES_DIR)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.home_dir.join(const_dirs::LOGS_DIR)
    }

    /// OCI images layers storage: ~/.boxlite/images/layers
    pub fn image_layers_dir(&self) -> PathBuf {
        self.images_dir().join(const_dirs::LAYERS_DIR)
    }

    /// OCI images manifests cache: ~/.boxlite/images/manifests
    pub fn image_manifests_dir(&self) -> PathBuf {
        self.images_dir().join(const_dirs::MANIFESTS_DIR)
    }

    /// Root directory for all box workspaces: ~/.boxlite/boxes
    /// Each box gets a subdirectory containing upper/work dirs for overlayfs
    pub fn boxes_dir(&self) -> PathBuf {
        self.home_dir.join(const_dirs::BOXES_DIR)
    }

    /// Temporary directory for transient files: ~/.boxlite/tmp
    /// Used for disk image creation and other operations that need
    /// temp files on the same filesystem as the final destination.
    pub fn temp_dir(&self) -> PathBuf {
        self.home_dir.join("tmp")
    }

    /// Initialize the filesystem structure.
    ///
    /// Creates necessary directories (home_dir, sockets, images, etc.).
    pub fn prepare(&self) -> BoxliteResult<()> {
        std::fs::create_dir_all(&self.home_dir)
            .map_err(|e| BoxliteError::Storage(format!("failed to create home: {e}")))?;

        let _ = std::fs::remove_dir_all(self.boxes_dir());
        std::fs::create_dir_all(self.boxes_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create boxes dir: {e}")))?;

        // Clean and recreate temp dir to avoid stale files from previous runs
        let _ = std::fs::remove_dir_all(self.temp_dir());
        std::fs::create_dir_all(self.temp_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create temp dir: {e}")))?;

        std::fs::create_dir_all(self.image_layers_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create layers dir: {e}")))?;

        std::fs::create_dir_all(self.image_manifests_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create manifests dir: {e}")))?;

        Ok(())
    }

    /// Create a box layout for a specific box ID.
    pub fn box_layout(&self, box_id: &str) -> BoxFilesystemLayout {
        BoxFilesystemLayout::new(self.boxes_dir().join(box_id))
    }

    /// Create an image layout for the images directory.
    pub fn image_layout(&self) -> ImageFilesystemLayout {
        ImageFilesystemLayout::new(self.images_dir())
    }
}

// ============================================================================
// BOX FILESYSTEM LAYOUT (per-box directory)
// ============================================================================

/// Filesystem layout for a single box directory.
///
/// Each box has its own directory containing:
/// - sockets/: Unix sockets for communication
/// - rw/: Writable layer for overlayfs
/// - disk.qcow2: Virtual disk for the box
#[derive(Clone, Debug)]
pub struct BoxFilesystemLayout {
    box_dir: PathBuf,
}

impl BoxFilesystemLayout {
    pub fn new(box_dir: PathBuf) -> Self {
        Self { box_dir }
    }

    /// Root directory for this box: ~/.boxlite/boxes/{box_id}
    pub fn root(&self) -> &Path {
        &self.box_dir
    }

    /// Sockets directory: ~/.boxlite/boxes/{box_id}/sockets
    pub fn sockets_dir(&self) -> PathBuf {
        self.box_dir.join(const_dirs::SOCKETS_DIR)
    }

    /// Unix socket path: ~/.boxlite/boxes/{box_id}/sockets/box.sock
    pub fn socket_path(&self) -> PathBuf {
        self.sockets_dir().join("box.sock")
    }

    /// Ready notification socket: ~/.boxlite/boxes/{box_id}/sockets/ready.sock
    ///
    /// Guest connects to this socket to signal it's ready to serve.
    pub fn ready_socket_path(&self) -> PathBuf {
        self.sockets_dir().join("ready.sock")
    }

    /// Writable layer directory for overlayfs: ~/.boxlite/boxes/{box_id}/rw
    pub fn rw_dir(&self) -> PathBuf {
        self.box_dir.join("rw")
    }

    /// Virtual disk path: ~/.boxlite/boxes/{box_id}/disk.qcow2
    pub fn disk_path(&self) -> PathBuf {
        self.box_dir.join("disk.qcow2")
    }

    /// Console output path: ~/.boxlite/boxes/{box_id}/console.log
    ///
    /// Captures kernel and init output for debugging.
    pub fn console_output_path(&self) -> PathBuf {
        self.box_dir.join("console.log")
    }

    /// Prepare the box directory structure.
    pub fn prepare(&self) -> BoxliteResult<()> {
        std::fs::create_dir_all(&self.box_dir)
            .map_err(|e| BoxliteError::Storage(format!("failed to create box dir: {e}")))?;

        std::fs::create_dir_all(self.sockets_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create sockets dir: {e}")))?;

        std::fs::create_dir_all(self.rw_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create rw dir: {e}")))?;

        Ok(())
    }

    /// Cleanup the box directory.
    pub fn cleanup(&self) -> BoxliteResult<()> {
        if self.box_dir.exists() {
            std::fs::remove_dir_all(&self.box_dir)
                .map_err(|e| BoxliteError::Storage(format!("failed to cleanup box dir: {e}")))?;
        }
        Ok(())
    }
}

// ============================================================================
// IMAGE FILESYSTEM LAYOUT (images directory)
// ============================================================================

/// Filesystem layout for OCI images storage.
///
/// Contains:
/// - layers/: Downloaded layer tarballs
/// - extracted/: Extracted layer directories
/// - disk-images/: Cached disk images for COW
/// - manifests/: Image manifests
/// - configs/: Image configs
#[derive(Clone, Debug)]
pub struct ImageFilesystemLayout {
    images_dir: PathBuf,
}

impl ImageFilesystemLayout {
    pub fn new(images_dir: PathBuf) -> Self {
        Self { images_dir }
    }

    /// Root directory: ~/.boxlite/images
    pub fn root(&self) -> &Path {
        &self.images_dir
    }

    /// Layers directory: ~/.boxlite/images/layers
    pub fn layers_dir(&self) -> PathBuf {
        self.images_dir.join(const_dirs::LAYERS_DIR)
    }

    /// Extracted layers directory: ~/.boxlite/images/extracted
    pub fn extracted_dir(&self) -> PathBuf {
        self.images_dir.join("extracted")
    }

    /// Disk images directory: ~/.boxlite/images/disk-images
    pub fn disk_images_dir(&self) -> PathBuf {
        self.images_dir.join("disk-images")
    }

    /// Manifests directory: ~/.boxlite/images/manifests
    pub fn manifests_dir(&self) -> PathBuf {
        self.images_dir.join(const_dirs::MANIFESTS_DIR)
    }

    /// Configs directory: ~/.boxlite/images/configs
    pub fn configs_dir(&self) -> PathBuf {
        self.images_dir.join("configs")
    }

    /// Prepare the images directory structure.
    pub fn prepare(&self) -> BoxliteResult<()> {
        std::fs::create_dir_all(self.layers_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create layers dir: {e}")))?;

        std::fs::create_dir_all(self.extracted_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create extracted dir: {e}")))?;

        std::fs::create_dir_all(self.disk_images_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create disk-images dir: {e}")))?;

        std::fs::create_dir_all(self.manifests_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create manifests dir: {e}")))?;

        std::fs::create_dir_all(self.configs_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create configs dir: {e}")))?;

        Ok(())
    }
}
