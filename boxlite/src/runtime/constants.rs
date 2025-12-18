//! Constants for BoxLite runtime
//!
//! Centralized location for all hardcoded values, paths, and configuration.
//! Host controls all paths - guest receives these via GuestInitRequest.

// Re-export shared constants from boxlite-core
pub use boxlite_shared::constants::{container, mount_tags, network};

/// Guest mount points (paths inside the guest)
/// These are sent to the guest via GuestInitRequest - guest doesn't hardcode them.
pub mod guest_paths {
    /// Base directory for workspace in guest
    pub const WORKSPACE_BASE: &str = "/boxlite/workspace";

    /// Rootfs mount point in guest (for merged strategy)
    pub const ROOTFS: &str = "/boxlite/workspace/rootfs";

    /// Directory for layer mounts in guest (for overlay strategy)
    pub const LAYERS_DIR: &str = "/boxlite/workspace/layers";

    /// Read-write directory in guest
    pub const RW_DIR: &str = "/boxlite/workspace/rw";

    /// Upper directory for overlayfs writes
    pub const UPPER_DIR: &str = "/boxlite/workspace/rw/upper";

    /// Work directory for overlayfs
    pub const WORK_DIR: &str = "/boxlite/workspace/rw/work";

    /// Merged overlayfs mount point (for overlay strategy)
    pub const MERGED_DIR: &str = "/boxlite/workspace/merged";

    /// Guest binary directory
    pub const BIN_DIR: &str = "/boxlite/bin";

    /// OCI container state directory
    pub const STATE_ROOT: &str = "/boxlite/workspace/rw/states";

    /// OCI container bundle directory
    pub const BUNDLE_ROOT: &str = "/boxlite/workspace/rw/bundles";

    /// Block device path in guest (virtio-blk) - data disk
    pub const DISK_DEVICE: &str = "/dev/vda";

    /// Mount point for writable disk in guest
    pub const DISK_MOUNT: &str = "/boxlite/workspace/disk";

    /// Block device path for rootfs disk (virtio-blk) - second disk
    pub const ROOTFS_DISK_DEVICE: &str = "/dev/vdb";

    /// Mount point for rootfs disk in guest
    pub const ROOTFS_DISK_MOUNT: &str = "/boxlite/workspace/rootfs";

    /// Block device path for init rootfs disk (virtio-blk) - third disk
    /// Used when init_rootfs uses disk-based strategy
    pub const INIT_DISK_DEVICE: &str = "/dev/vdc";

    /// Mount point for init rootfs disk before pivot_root
    pub const INIT_DISK_MOUNT: &str = "/mnt/init";
}

pub mod envs {
    pub const BOXLITE_HOME: &str = "BOXLITE_HOME";
}

/// Container images used by the runtime
pub mod images {
    /// Default container image when none is specified
    pub const DEFAULT: &str = "alpine:latest";

    /// Base image for VM init rootfs (must include mkfs.ext4 for disk formatting)
    pub const INIT_ROOTFS: &str = "debian:bookworm-slim";
}

/// Directory structure constants
pub mod dirs {
    /// Base directory name for BoxLite data
    pub const BOXLITE_DIR: &str = ".boxlite";

    /// Subdirectory for images layers
    pub const IMAGES_DIR: &str = "images";

    /// Subdirectory for individual layer storage
    pub const LAYERS_DIR: &str = "layers";

    /// Subdirectory for images manifests
    pub const MANIFESTS_DIR: &str = "manifests";

    /// Subdirectory for running boxes
    pub const BOXES_DIR: &str = "boxes";

    /// Subdirectory for Unix domain sockets
    pub const SOCKETS_DIR: &str = "sockets";

    /// Subdirectory for overlayfs upper layer (Linux only)
    pub const UPPER_DIR: &str = "upper";

    /// Subdirectory for overlayfs work directory (Linux only)
    pub const WORK_DIR: &str = "work";

    /// Subdirectory for writable layer (per box)
    pub const RW_DIR: &str = "rw";

    /// Subdirectory for log files
    pub const LOGS_DIR: &str = "logs";

    /// Subdirectory for disk images
    pub const DISKS_DIR: &str = "disks";
}

/// Filesystem and mount options
pub mod fs_options {
    /// Default tmpfs size for writable layer (in MB)
    pub const TMPFS_SIZE_MB: usize = 1024;

    /// Overlayfs mount options
    pub const OVERLAYFS_OPTIONS: &[&str] =
        &["metacopy=off", "redirect_dir=off", "index=off", "xino=off"];
}

/// Virtual machine resource defaults
pub mod vm_defaults {
    /// Default number of CPUs allocated to a Box
    pub const DEFAULT_CPUS: u8 = 1;

    /// Default memory in MiB allocated to a Box
    pub const DEFAULT_MEMORY_MIB: u32 = 2048;
}

/// File naming patterns
pub mod filenames {
    use std::path::{Path, PathBuf};

    /// Lock file name
    pub const LOCK_FILE: &str = ".lock";

    pub fn box_home(home_dir: &Path, box_id: &str) -> PathBuf {
        home_dir.join(super::dirs::BOXES_DIR).join(box_id)
    }

    /// Get full path for Unix socket
    pub fn unix_socket_path(home_dir: &Path, box_id: &str) -> PathBuf {
        box_home(home_dir, box_id)
            .join(super::dirs::SOCKETS_DIR)
            .join("box.sock")
    }
}
