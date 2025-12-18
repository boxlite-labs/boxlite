//! Filesystem layout definitions shared between host and guest.
//!
//! This module provides layout structs for the shared filesystem pattern:
//! - `SharedGuestLayout`: Layout for the shared directory (virtiofs mount)
//! - `SharedContainerLayout`: Per-container directory layout within shared/
//!
//! Lives in boxlite-shared so both host and guest can use these definitions.

use std::path::{Path, PathBuf};

// ============================================================================
// CONSTANTS
// ============================================================================

/// Shared filesystem directory names.
pub mod dirs {
    /// Host preparation directory (host writes here)
    pub const MOUNTS: &str = "mounts";

    /// Guest-visible directory (bind mount target, read-only on Linux)
    pub const SHARED: &str = "shared";

    /// Containers subdirectory
    pub const CONTAINERS: &str = "containers";

    /// Container rootfs directory name (all rootfs strategies mount here)
    pub const ROOTFS: &str = "rootfs";

    /// Overlayfs directory name (contains upper/ and work/)
    pub const OVERLAYFS: &str = "overlayfs";

    /// Overlayfs upper directory name
    pub const UPPER: &str = "upper";

    /// Overlayfs work directory name
    pub const WORK: &str = "work";
}

/// Guest base path (FHS-compliant).
pub const GUEST_BASE: &str = "/run/boxlite";

// ============================================================================
// SHARED CONTAINER LAYOUT (per-container directories)
// ============================================================================

/// Per-container directory layout within the shared filesystem.
///
/// Represents the directory structure for a single container:
/// ```text
/// {root}/                    # shared/containers/{cid}/
/// ├── overlayfs/
/// │   ├── upper/             # Overlayfs upper (writable layer)
/// │   └── work/              # Overlayfs work directory
/// └── rootfs/                # All rootfs strategies mount here
/// ```
#[derive(Clone, Debug)]
pub struct SharedContainerLayout {
    root: PathBuf,
}

impl SharedContainerLayout {
    /// Create a container layout with the given root path.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Root directory of this container: shared/containers/{cid}
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Overlayfs directory: {root}/overlayfs
    pub fn overlayfs_dir(&self) -> PathBuf {
        self.root.join(dirs::OVERLAYFS)
    }

    /// Upper directory: {root}/overlayfs/upper
    ///
    /// Writable layer for overlayfs.
    pub fn upper_dir(&self) -> PathBuf {
        self.overlayfs_dir().join(dirs::UPPER)
    }

    /// Work directory: {root}/overlayfs/work
    ///
    /// Overlayfs work directory.
    pub fn work_dir(&self) -> PathBuf {
        self.overlayfs_dir().join(dirs::WORK)
    }

    /// Rootfs directory: {root}/rootfs
    ///
    /// All rootfs strategies (merged, overlayfs, disk image) mount here.
    /// Guest bind mounts /run/boxlite/{cid}/rootfs/ to this location.
    pub fn rootfs_dir(&self) -> PathBuf {
        self.root.join(dirs::ROOTFS)
    }

    /// Prepare container directories.
    pub fn prepare(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.upper_dir())?;
        std::fs::create_dir_all(self.work_dir())?;
        std::fs::create_dir_all(self.rootfs_dir())?;
        Ok(())
    }
}

// ============================================================================
// SHARED GUEST LAYOUT (shared directory root)
// ============================================================================

/// Shared directory layout - identical structure on host and guest.
///
/// This struct represents the directory structure under:
/// - Host: `~/.boxlite/boxes/{box-id}/mounts/`
/// - Guest: `/run/boxlite/shared/`
///
/// The structure is:
/// ```text
/// {base}/
/// └── containers/
///     └── {cid}/              # SharedContainerLayout
///         ├── overlayfs/{upper,work}
///         └── rootfs/
/// ```
///
/// # Example
///
/// ```
/// use boxlite_shared::layout::SharedGuestLayout;
///
/// // Host usage
/// let host_layout = SharedGuestLayout::new("/home/user/.boxlite/boxes/abc123/mounts");
///
/// // Guest usage
/// let guest_layout = SharedGuestLayout::new("/run/boxlite/shared");
///
/// // Both have identical container paths relative to base
/// let host_container = host_layout.container("main");
/// let guest_container = guest_layout.container("main");
/// assert!(host_container.rootfs_dir().ends_with("containers/main/rootfs"));
/// assert!(guest_container.rootfs_dir().ends_with("containers/main/rootfs"));
/// ```
#[derive(Clone, Debug)]
pub struct SharedGuestLayout {
    base: PathBuf,
}

impl SharedGuestLayout {
    /// Create a shared layout with the given base path.
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// Base directory of this shared layout.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Containers directory: {base}/containers
    pub fn containers_dir(&self) -> PathBuf {
        self.base.join(dirs::CONTAINERS)
    }

    /// Get layout for a specific container.
    pub fn container(&self, container_id: &str) -> SharedContainerLayout {
        SharedContainerLayout::new(self.containers_dir().join(container_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // SharedContainerLayout tests
    // ========================================================================

    #[test]
    fn test_container_layout_paths() {
        let container = SharedContainerLayout::new("/test/shared/containers/main");

        assert_eq!(
            container.root().to_str().unwrap(),
            "/test/shared/containers/main"
        );
        assert_eq!(
            container.overlayfs_dir().to_str().unwrap(),
            "/test/shared/containers/main/overlayfs"
        );
        assert_eq!(
            container.upper_dir().to_str().unwrap(),
            "/test/shared/containers/main/overlayfs/upper"
        );
        assert_eq!(
            container.work_dir().to_str().unwrap(),
            "/test/shared/containers/main/overlayfs/work"
        );
        assert_eq!(
            container.rootfs_dir().to_str().unwrap(),
            "/test/shared/containers/main/rootfs"
        );
    }

    // ========================================================================
    // SharedGuestLayout tests
    // ========================================================================

    #[test]
    fn test_shared_guest_layout_paths() {
        let layout = SharedGuestLayout::new("/test/shared");

        assert_eq!(layout.base().to_str().unwrap(), "/test/shared");
        assert_eq!(
            layout.containers_dir().to_str().unwrap(),
            "/test/shared/containers"
        );
    }

    #[test]
    fn test_shared_guest_layout_container() {
        let layout = SharedGuestLayout::new("/test/shared");
        let container = layout.container("main");

        assert_eq!(
            container.overlayfs_dir().to_str().unwrap(),
            "/test/shared/containers/main/overlayfs"
        );
        assert_eq!(
            container.rootfs_dir().to_str().unwrap(),
            "/test/shared/containers/main/rootfs"
        );
    }

    #[test]
    fn test_shared_guest_layout_host_guest_identical() {
        // Host and guest have identical structure under their respective bases
        let host = SharedGuestLayout::new("/home/user/.boxlite/boxes/abc/mounts");
        let guest = SharedGuestLayout::new("/run/boxlite/shared");

        // Relative paths are identical
        let host_rootfs_dir = host.container("main").rootfs_dir();
        let guest_rootfs_dir = guest.container("main").rootfs_dir();
        let host_rel = host_rootfs_dir.strip_prefix(host.base()).unwrap();
        let guest_rel = guest_rootfs_dir.strip_prefix(guest.base()).unwrap();
        assert_eq!(host_rel, guest_rel);
    }
}
