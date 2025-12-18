use crate::runtime::constants::dirs as const_dirs;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use boxlite_shared::layout::{GUEST_BASE, SharedGuestLayout, dirs as shared_dirs};
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
/// - mounts/: Host preparation area (writable by host)
/// - shared/: Guest-visible directory (bind mount or symlink to mounts/)
/// - disk.qcow2: Virtual disk for the box
///
/// The mounts/ and shared/ directories follow this pattern:
/// - Host writes to mounts/containers/{cid}/...
/// - Guest sees shared/containers/{cid}/... via virtio-fs
/// - On Linux: shared/ is a read-only bind mount of mounts/
/// - On macOS: shared/ is a symlink to mounts/ (workaround)
///
/// # Directory Structure
///
/// ```text
/// ~/.boxlite/boxes/{box_id}/
/// ├── sockets/
/// │   ├── box.sock        # gRPC communication
/// │   └── ready.sock      # Ready notification
/// ├── mounts/             # Host preparation (SharedGuestLayout)
/// │   └── containers/
/// │       └── {cid}/
/// │           ├── image/      # Container image (lowerdir)
/// │           ├── rw/
/// │           │   ├── upper/  # Overlayfs upper
/// │           │   └── work/   # Overlayfs work
/// │           └── rootfs/     # Final rootfs (overlayfs merged)
/// ├── shared/             # Guest-visible (bind mount/symlink → mounts/)
/// ├── disk.qcow2          # Data disk
/// └── console.log         # Kernel/init output
/// ```
#[derive(Clone, Debug)]
pub struct BoxFilesystemLayout {
    box_dir: PathBuf,
    /// SharedGuestLayout for the mounts/ directory (host writes here).
    shared_layout: SharedGuestLayout,
}

impl BoxFilesystemLayout {
    pub fn new(box_dir: PathBuf) -> Self {
        let shared_layout = SharedGuestLayout::new(box_dir.join(shared_dirs::MOUNTS));
        Self {
            box_dir,
            shared_layout,
        }
    }

    /// Root directory for this box: ~/.boxlite/boxes/{box_id}
    pub fn root(&self) -> &Path {
        &self.box_dir
    }

    // ========================================================================
    // SOCKETS
    // ========================================================================

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

    // ========================================================================
    // MOUNTS AND SHARED
    // ========================================================================

    /// SharedGuestLayout for the mounts/ directory (host-side paths).
    ///
    /// Host preparation area. Host writes container images and rw layers here.
    /// Returns the SharedGuestLayout for accessing container directories.
    pub fn shared_layout(&self) -> &SharedGuestLayout {
        &self.shared_layout
    }

    /// SharedGuestLayout for guest-side paths.
    ///
    /// Returns a layout with base `/run/boxlite/shared` for constructing
    /// paths that will be used inside the guest VM.
    pub fn guest_shared_layout(&self) -> SharedGuestLayout {
        let guest_base = format!("{}/{}", GUEST_BASE, shared_dirs::SHARED);
        SharedGuestLayout::new(guest_base)
    }

    /// Mounts directory path: ~/.boxlite/boxes/{box_id}/mounts
    pub fn mounts_dir(&self) -> &Path {
        self.shared_layout.base()
    }

    /// Shared directory: ~/.boxlite/boxes/{box_id}/shared
    ///
    /// Guest-visible directory. On Linux, this is a read-only bind mount of mounts/.
    /// On macOS, this is a symlink to mounts/ (workaround).
    ///
    /// This directory is exposed to the guest via virtio-fs with tag "shared".
    pub fn shared_dir(&self) -> PathBuf {
        self.box_dir.join(shared_dirs::SHARED)
    }

    // ========================================================================
    // DISK AND CONSOLE
    // ========================================================================

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

    // ========================================================================
    // PREPARATION AND CLEANUP
    // ========================================================================

    /// Prepare the box directory structure.
    ///
    /// Creates:
    /// - sockets/
    /// - mounts/ (via SharedGuestLayout base)
    ///
    /// Note: shared/ is NOT created here - it will be created as a bind mount
    /// (Linux) or symlink (macOS) in the filesystem stage.
    pub fn prepare(&self) -> BoxliteResult<()> {
        std::fs::create_dir_all(&self.box_dir)
            .map_err(|e| BoxliteError::Storage(format!("failed to create box dir: {e}")))?;

        std::fs::create_dir_all(self.sockets_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create sockets dir: {e}")))?;

        std::fs::create_dir_all(self.mounts_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create mounts dir: {e}")))?;

        // shared/ is created by create_bind_mount() - don't create it here
        // On Linux: bind mount from mounts/
        // On macOS: symlink to mounts/

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
