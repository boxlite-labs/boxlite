use crate::net::socket_path::BoxSockets;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use boxlite_shared::layout::{SharedGuestLayout, dirs as shared_dirs};
use std::path::{Path, PathBuf};

/// Directory structure constants
pub mod dirs {
    /// Base directory name for BoxLite data
    pub const BOXLITE_DIR: &str = ".boxlite";

    pub const DB_DIR: &str = "db";

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

    /// Subdirectory for overlayfs (per container)
    pub const OVERLAYFS_DIR: &str = "overlayfs";

    /// Subdirectory for log files
    pub const LOGS_DIR: &str = "logs";

    /// Shim process identity file.
    pub const SHIM_PID_FILE: &str = "shim.pid";

    /// Subdirectory for disk images
    pub const DISKS_DIR: &str = "disks";

    /// Subdirectory for per-entity locks
    pub const LOCKS_DIR: &str = "locks";
}

/// Configuration for filesystem layout behavior.
///
/// Controls platform-specific filesystem features like bind mounts.
#[derive(Clone, Debug, Default)]
pub struct FsLayoutConfig {
    /// Whether bind mount is supported on this platform.
    ///
    /// - `true`: Use bind mount (mounts/ → shared/), expose shared/ to guest
    /// - `false`: Skip bind mount, expose mounts/ directly to guest
    bind_mount_supported: bool,
}

impl FsLayoutConfig {
    /// Create a new config with bind mount support enabled.
    pub fn with_bind_mount() -> Self {
        Self {
            bind_mount_supported: true,
        }
    }

    /// Create a new config with bind mount support disabled.
    pub fn without_bind_mount() -> Self {
        Self {
            bind_mount_supported: false,
        }
    }

    /// Check if bind mount is supported.
    pub fn is_bind_mount_supported(&self) -> bool {
        self.bind_mount_supported
    }
}

// ============================================================================
// FILESYSTEM LAYOUT (home directory)
// ============================================================================

/// Filesystem layout for the BoxLite home directory.
///
/// All runtime data lives under a single home directory (`~/.boxlite/` by default,
/// overridable via `BOXLITE_HOME`). This layout manages the top-level structure
/// and delegates per-box and per-image layouts to their respective types.
///
/// # Directory Structure
///
/// ```text
/// ~/.boxlite/                              # Home directory (BOXLITE_HOME)
/// ├── db/                                  # SQLite databases
/// ├── images/                              # OCI image cache (ImageFilesystemLayout)
/// │   ├── layers/                              # Downloaded layer tarballs
/// │   ├── extracted/                           # Extracted layer directories
/// │   ├── disk-images/                         # Cached ext4 disk images for COW
/// │   ├── manifests/                           # Image manifests
/// │   ├── configs/                             # Image configs
/// │   └── local/                               # Local OCI bundle cache
/// │       └── {path_hash}-{manifest_short}/
/// ├── boxes/                               # Per-box directories (BoxFilesystemLayout)
/// │   └── {box_id}/                            # See BoxFilesystemLayout
/// ├── volumes/                             # Named volumes (LocalNamedVolumeStore)
/// │   └── {volume_id}/                         # One volume
/// │       ├── .metadata.json                   # Its name and creation time
/// │       └── _data/                           # Payload, mounted into a box
/// ├── bases/                               # Flat backing files (nanoid-named)
/// │   ├── {nanoid}.qcow2                       # Snapshot / clone base container disk
/// │   └── {nanoid}.ext4                        # Guest rootfs cache
/// ├── locks/                               # Per-entity lock files
/// ├── logs/                                # Home-level logs
/// └── tmp/                                 # Transient temp files (same-fs for rename)
/// ```
///
/// The `tmp/`, `bases/`, and `images/disk-images/` directories **must** reside on the
/// same filesystem to allow atomic `rename(2)` during staged install operations.
#[derive(Clone, Debug)]
pub struct FilesystemLayout {
    home_dir: PathBuf,
    config: FsLayoutConfig,
}

impl FilesystemLayout {
    pub fn new(home_dir: PathBuf, config: FsLayoutConfig) -> Self {
        Self { home_dir, config }
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub fn db_dir(&self) -> PathBuf {
        self.home_dir.join(dirs::DB_DIR)
    }

    pub fn images_dir(&self) -> PathBuf {
        self.home_dir.join(dirs::IMAGES_DIR)
    }

    pub fn logs_dir(&self) -> PathBuf {
        Self::logs_dir_for(&self.home_dir)
    }

    /// Compute the logs dir for a given home directory without constructing
    /// a full `FilesystemLayout`. Used by logging init paths that run before
    /// the runtime (and thus before the layout) is built.
    pub fn logs_dir_for(home_dir: &Path) -> PathBuf {
        home_dir.join(dirs::LOGS_DIR)
    }

    /// OCI images layers storage: ~/.boxlite/images/layers
    pub fn image_layers_dir(&self) -> PathBuf {
        self.images_dir().join(dirs::LAYERS_DIR)
    }

    /// OCI images manifests cache: ~/.boxlite/images/manifests
    pub fn image_manifests_dir(&self) -> PathBuf {
        self.images_dir().join(dirs::MANIFESTS_DIR)
    }

    /// Root directory for all box workspaces: ~/.boxlite/boxes
    /// Each box gets a subdirectory containing upper/work dirs for overlayfs
    pub fn boxes_dir(&self) -> PathBuf {
        self.home_dir.join(dirs::BOXES_DIR)
    }

    /// Bases directory: ~/.boxlite/bases
    ///
    /// Flat directory of immutable backing files (snapshots, clone bases, rootfs cache).
    /// Each file is named with a nanoid(8) identifier and tracked in the `base_disk` table.
    pub fn bases_dir(&self) -> PathBuf {
        self.home_dir.join("bases")
    }

    /// Per-entity locks directory: ~/.boxlite/locks
    ///
    /// Contains lock files managed by FileLockManager for multiprocess-safe
    /// locking of individual entities (boxes, volumes, etc.).
    pub fn locks_dir(&self) -> PathBuf {
        self.home_dir.join(dirs::LOCKS_DIR)
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

        std::fs::create_dir_all(self.boxes_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create boxes dir: {e}")))?;

        std::fs::create_dir_all(self.bases_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create bases dir: {e}")))?;

        std::fs::create_dir_all(self.temp_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create temp dir: {e}")))?;

        std::fs::create_dir_all(self.image_layers_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create layers dir: {e}")))?;

        std::fs::create_dir_all(self.image_manifests_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create manifests dir: {e}")))?;

        std::fs::create_dir_all(self.image_layout().disk_images_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create disk-images dir: {e}")))?;

        self.validate_same_filesystem()?;

        Ok(())
    }

    /// Validate that temp, bases, and disk-images directories are on the same filesystem.
    ///
    /// This is required for atomic `rename(2)` in staged install operations.
    /// Refuse to start if directories span multiple filesystems.
    fn validate_same_filesystem(&self) -> BoxliteResult<()> {
        use std::os::unix::fs::MetadataExt;

        let temp_dev = std::fs::metadata(self.temp_dir())
            .map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to stat temp dir {}: {}",
                    self.temp_dir().display(),
                    e
                ))
            })?
            .dev();

        let bases_dev = std::fs::metadata(self.bases_dir())
            .map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to stat bases dir {}: {}",
                    self.bases_dir().display(),
                    e
                ))
            })?
            .dev();

        let images_dev = std::fs::metadata(self.image_layout().disk_images_dir())
            .map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to stat disk-images dir {}: {}",
                    self.image_layout().disk_images_dir().display(),
                    e
                ))
            })?
            .dev();

        if temp_dev != bases_dev || temp_dev != images_dev {
            return Err(BoxliteError::Storage(format!(
                "tmp, bases, and disk-images directories must be on the same filesystem \
                 for atomic rename. Found devices: tmp={}, bases={}, disk-images={}. \
                 Check your BOXLITE_HOME configuration.",
                temp_dev, bases_dev, images_dev
            )));
        }

        Ok(())
    }

    /// Create a box layout for a specific box ID.
    pub fn box_layout(
        &self,
        box_id: &str,
        isolate_mounts: bool,
    ) -> BoxliteResult<BoxFilesystemLayout> {
        let effective_isolate = isolate_mounts && self.config.is_bind_mount_supported();

        if isolate_mounts && !effective_isolate {
            tracing::warn!(
                "Mount isolation requested but bind mounts are not supported on this system. \
                 Falling back to shared directory without isolation."
            );
        }

        Ok(BoxFilesystemLayout::new(
            self.boxes_dir().join(box_id),
            self.config.clone(),
            effective_isolate,
        ))
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
/// - tmp/: Per-box temp directory for shim/libkrun transient files
/// - boot/: Immutable generations of staged custom kernel assets
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
/// ├── tmp/                # Per-box temp files for shim/libkrun
/// ├── boot/               # Custom direct-boot assets (when configured)
/// │   ├── current.json        # Atomically published active generation
/// │   └── generations/
/// │       └── {generation}/
/// │           ├── kernel
/// │           └── initramfs   # Optional
/// ├── mounts/             # Host preparation (SharedGuestLayout)
/// │   └── containers/
/// │       └── {cid}/
/// │           ├── image/      # Container image (lowerdir)
/// │           ├── oberlayfs/
/// │           │   ├── upper/  # Overlayfs upper
/// │           │   └── work/   # Overlayfs work
/// │           └── rootfs/     # Final rootfs (overlayfs merged)
/// ├── shared/             # Guest-visible (ro bind mount → mounts/)
/// ├── logs/               # Per-box logging
/// │   ├── boxlite-shim.log  # Shim tracing output
/// │   └── console.log       # Kernel/init output
/// └── disks/              # Disk images
///     ├── disk.qcow2          # Data disk (container rootfs COW disk)
///     └── guest-rootfs.qcow2  # Guest rootfs COW overlay
/// ```
#[derive(Clone, Debug)]
pub struct BoxFilesystemLayout {
    box_dir: PathBuf,
    /// SharedGuestLayout for the mounts/ directory (host writes here).
    shared_layout: SharedGuestLayout,
    /// Filesystem layout configuration.
    config: FsLayoutConfig,
    /// Whether to use bind mount isolation for the mounts directory.
    /// Only effective when bind mounts are supported on the system.
    isolate_mounts: bool,
}

impl BoxFilesystemLayout {
    pub fn new(box_dir: PathBuf, config: FsLayoutConfig, isolate_mounts: bool) -> Self {
        let shared_layout = SharedGuestLayout::new(box_dir.join(shared_dirs::MOUNTS));
        Self {
            box_dir,
            shared_layout,
            config,
            isolate_mounts,
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
    ///
    /// This is the REAL directory (for mkdir, sandbox policies, inspection).
    /// Paths handed to `bind()`/`connect()` come from [`Self::sockets`] —
    /// every socket is bound through the short `/tmp/bl-{uid}/{box_id}`
    /// symlink to stay inside the `sun_path` limit.
    pub fn sockets_dir(&self) -> PathBuf {
        self.box_dir.join(dirs::SOCKETS_DIR)
    }

    /// The socket-path authority for this box (binding paths, symlink
    /// lifecycle, sandbox policy paths). See [`BoxSockets`].
    pub fn sockets(&self) -> BoxSockets {
        // Box ids are minted ASCII (see runtime::id), so lossy is exact.
        let box_id = self
            .box_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        BoxSockets::new(box_id, self.sockets_dir())
    }

    /// gRPC control socket binding path (see [`BoxSockets::box_sock`]).
    pub fn socket_path(&self) -> PathBuf {
        self.sockets().box_sock()
    }

    /// Guest-ready notification socket binding path
    /// (see [`BoxSockets::ready_sock`]).
    pub fn ready_socket_path(&self) -> PathBuf {
        self.sockets().ready_sock()
    }

    /// Network backend socket binding path (gvproxy; libkrun derives a
    /// sibling `net.sock-krun.sock` — see [`BoxSockets::net_backend_sock`]).
    pub fn net_backend_socket_path(&self) -> PathBuf {
        self.sockets().net_backend_sock()
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

    /// Directory for host-side file preparation, exposed to guest via virtio-fs.
    ///
    /// The bind mount pattern (mounts/ → shared/) serves two purposes:
    /// 1. Host writes to mounts/ with full read-write access
    /// 2. Guest sees shared/ as read-only (bind mount with MS_RDONLY)
    ///
    /// This prevents guest from modifying host-prepared files while allowing
    /// the host to update content at any time.
    ///
    /// Returns the appropriate directory based on bind mount configuration:
    /// - `is_bind_mount_supported && isolate_mounts = true`: Returns mounts/ (host writes here, bind-mounted to shared/)
    /// - Otherwise: Returns shared/ directly (no bind mount available or not requested)
    pub fn mounts_dir(&self) -> PathBuf {
        if self.config.is_bind_mount_supported() && self.isolate_mounts {
            self.shared_layout.base().to_path_buf()
        } else {
            self.shared_dir()
        }
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

    /// The container's exit file: shared/containers/{cid}/exit.json.
    ///
    /// Written by the guest when the box's main command exits. Distinct from
    /// [`Self::exit_file_path`], which is the *shim's* exit record used for
    /// crash reporting — this one is the container's, and it is the same path
    /// the guest computes from its own side of the virtiofs mount.
    pub fn container_exit_file(&self, container_id: &str) -> PathBuf {
        boxlite_shared::layout::SharedGuestLayout::new(self.shared_dir())
            .container(container_id)
            .exit_file()
    }

    // BIN AND LOGS (jailer isolation)
    // ========================================================================

    /// Bin directory: ~/.boxlite/boxes/{box_id}/bin
    ///
    /// Contains the shim binary and bundled libraries, copied (or reflinked)
    /// by the jailer for memory isolation between boxes.
    pub fn bin_dir(&self) -> PathBuf {
        self.box_dir.join("bin")
    }

    /// Boot assets directory: ~/.boxlite/boxes/{box_id}/boot
    ///
    /// Contains immutable generations of a custom kernel and optional
    /// initramfs. `current.json` publishes one complete generation atomically,
    /// and the shim receives read-only access to the directory.
    pub fn boot_dir(&self) -> PathBuf {
        self.box_dir.join("boot")
    }

    /// CA directory: ~/.boxlite/boxes/{box_id}/ca
    ///
    /// Stores ephemeral MITM CA cert+key for secret substitution.
    /// NOT under shared/ — the guest must not read the private key.
    /// Files: cert.pem (0644), key.pem (0600).
    pub fn ca_dir(&self) -> PathBuf {
        self.box_dir.join("ca")
    }

    /// Per-box logs directory: ~/.boxlite/boxes/{box_id}/logs
    ///
    /// Shim writes its logs here instead of the shared home_dir/logs/,
    /// so the sandbox doesn't need access to any home_dir paths for writing.
    pub fn logs_dir(&self) -> PathBuf {
        self.box_dir.join("logs")
    }

    /// Per-box temp directory: ~/.boxlite/boxes/{box_id}/tmp
    ///
    /// Used for shim/libkrun transient files when jailer is enabled with the
    /// built-in seatbelt profile.
    pub fn tmp_dir(&self) -> PathBuf {
        self.box_dir.join("tmp")
    }

    // ========================================================================
    // DISK AND CONSOLE
    // ========================================================================

    /// Disks directory: ~/.boxlite/boxes/{box_id}/disks
    pub fn disks_dir(&self) -> PathBuf {
        self.box_dir.join(dirs::DISKS_DIR)
    }

    /// Virtual disk path: ~/.boxlite/boxes/{box_id}/disks/disk.qcow2
    pub fn disk_path(&self) -> PathBuf {
        self.disks_dir()
            .join(crate::disk::constants::filenames::CONTAINER_DISK)
    }

    /// Guest rootfs disk path: ~/.boxlite/boxes/{box_id}/disks/guest-rootfs.qcow2
    pub fn guest_rootfs_disk_path(&self) -> PathBuf {
        self.disks_dir()
            .join(crate::disk::constants::filenames::GUEST_ROOTFS_DISK)
    }

    /// Console output path: ~/.boxlite/boxes/{box_id}/logs/console.log
    ///
    /// Captures kernel and init output for debugging.
    /// Lives inside `logs/` so the sandbox grants it via the `logs/` [RW subpath].
    pub fn console_output_path(&self) -> PathBuf {
        self.logs_dir().join("console.log")
    }

    /// PID file path: ~/.boxlite/boxes/{box_id}/shim.pid
    ///
    /// Written by the shim process in pre_exec (after fork, before exec).
    /// This is the single source of truth for the shim process PID.
    /// Database PID is a cache that can be reconstructed from this file.
    pub fn pid_file_path(&self) -> PathBuf {
        self.box_dir.join(dirs::SHIM_PID_FILE)
    }

    /// Exit file path: ~/.boxlite/boxes/{box_id}/exit
    ///
    /// Written by the shim process on exit (normal or panic).
    /// Format: First line is exit code, subsequent lines contain error details.
    /// Follows Podman's conmon pattern for capturing exit information.
    pub fn exit_file_path(&self) -> PathBuf {
        self.box_dir.join("exit")
    }

    /// Archived exit file from the previous lifecycle: `~/.boxlite/boxes/{box_id}/exit.previous`.
    ///
    /// Single-slot rotation: a successful start or recovery-with-alive-shim
    /// renames `exit` → `exit.previous`, preserving the prior crash record
    /// while freeing the canonical slot for the next crash.
    pub fn exit_previous_path(&self) -> PathBuf {
        self.box_dir.join("exit.previous")
    }

    /// Stderr file path: ~/.boxlite/boxes/{box_id}/shim.stderr
    ///
    /// Captures libkrun stderr output for crash diagnostics.
    /// The signal handler reads this and includes content in exit file.
    pub fn stderr_file_path(&self) -> PathBuf {
        self.box_dir.join("shim.stderr")
    }

    // ========================================================================
    // PREPARATION AND CLEANUP
    // ========================================================================

    /// Prepare the box directory structure.
    ///
    /// Creates:
    /// - sockets/
    /// - tmp/
    /// - mounts/ (via SharedGuestLayout base)
    /// - logs/
    ///
    /// Note: shared/ is NOT created here - it will be created as a bind mount
    /// (Linux) or symlink (macOS) in the filesystem stage.
    pub fn prepare(&self) -> BoxliteResult<()> {
        std::fs::create_dir_all(&self.box_dir)
            .map_err(|e| BoxliteError::Storage(format!("failed to create box dir: {e}")))?;

        std::fs::create_dir_all(self.sockets_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create sockets dir: {e}")))?;

        // All sockets bind through the short /tmp symlink (sun_path limit) —
        // it must exist BEFORE anything binds, or boot fails on ENOENT.
        self.sockets().ensure()?;

        std::fs::create_dir_all(self.tmp_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create tmp dir: {e}")))?;

        std::fs::create_dir_all(self.disks_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create disks dir: {e}")))?;

        std::fs::create_dir_all(self.mounts_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create mounts dir: {e}")))?;

        std::fs::create_dir_all(self.logs_dir())
            .map_err(|e| BoxliteError::Storage(format!("failed to create logs dir: {e}")))?;

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
        self.sockets().remove();
        Ok(())
    }
}

// ============================================================================
// IMAGE FILESYSTEM LAYOUT (images directory)
// ============================================================================

/// Filesystem layout for OCI images storage.
///
/// Manages the `~/.boxlite/images/` subtree where all OCI image data is cached.
/// Layer tarballs, extracted directories, and pre-built disk images are stored
/// here to avoid re-downloading and re-building on subsequent box creation.
///
/// # Directory Structure
///
/// ```text
/// ~/.boxlite/images/
/// ├── layers/                      # Downloaded layer tarballs (sha256-named)
/// ├── extracted/                   # Extracted layer directories
/// ├── disk-images/                 # Cached ext4 disk images for COW overlays
/// ├── manifests/                   # Image manifest JSON files
/// ├── configs/                     # Image config JSON files
/// └── local/                       # Local OCI bundle cache
///     └── {path_hash}-{manifest_short}/  # Per-bundle isolated cache
/// ```
#[derive(Clone, Debug)]
pub struct ImageFilesystemLayout {
    images_dir: PathBuf,
}

impl ImageFilesystemLayout {
    pub fn new(images_dir: PathBuf) -> Self {
        Self { images_dir }
    }

    /// Local bundle cache directory: `~/.boxlite/images/local/{path_hash}-{manifest_short}`
    ///
    /// Computes isolated cache dir for a local OCI bundle. Each bundle gets a unique
    /// namespace based on both its path AND manifest digest, ensuring cache invalidation
    /// when bundle content changes.
    ///
    /// # Arguments
    /// * `bundle_path` - Path to the OCI bundle directory
    /// * `manifest_digest` - Manifest digest (e.g., "sha256:abc123...")
    pub fn local_bundle_cache_dir(&self, bundle_path: &Path, manifest_digest: &str) -> PathBuf {
        use sha2::{Digest, Sha256};

        // Hash the bundle path for location identity
        let path_str = bundle_path.to_string_lossy();
        let path_hash = Sha256::digest(path_str.as_bytes());
        let path_short = hex::encode(path_hash).chars().take(8).collect::<String>();

        // Extract short manifest digest for content identity
        let manifest_short = manifest_digest
            .strip_prefix("sha256:")
            .unwrap_or(manifest_digest);
        let manifest_short = &manifest_short[..8.min(manifest_short.len())];

        self.images_dir
            .join("local")
            .join(format!("{}-{}", path_short, manifest_short))
    }

    /// Root directory: ~/.boxlite/images
    pub fn root(&self) -> &Path {
        &self.images_dir
    }

    /// Layers directory: ~/.boxlite/images/layers
    pub fn layers_dir(&self) -> PathBuf {
        self.images_dir.join(dirs::LAYERS_DIR)
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
        self.images_dir.join(dirs::MANIFESTS_DIR)
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

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_bundle_cache_dir_format() {
        let layout = ImageFilesystemLayout::new(PathBuf::from("/images"));

        let cache_dir =
            layout.local_bundle_cache_dir(Path::new("/my/bundle"), "sha256:abc123def456789");

        // Should be under /images/local/
        assert!(cache_dir.starts_with("/images/local/"));

        // Format: {path_hash}-{manifest_short}
        let dir_name = cache_dir.file_name().unwrap().to_str().unwrap();
        assert!(
            dir_name.contains('-'),
            "should have format path_hash-manifest_short"
        );

        // Path hash is 8 chars, manifest short is 8 chars
        let parts: Vec<&str> = dir_name.split('-').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].len(), 8, "path hash should be 8 chars");
        assert_eq!(parts[1].len(), 8, "manifest short should be 8 chars");
    }

    #[test]
    fn test_local_bundle_cache_invalidation_on_content_change() {
        // This test verifies that when bundle content changes (same path,
        // different manifest), a NEW cache directory is used.
        let layout = ImageFilesystemLayout::new(PathBuf::from("/images"));
        let bundle_path = Path::new("/my/bundle");

        // Original bundle version (realistic hex digest)
        let cache_v1 = layout.local_bundle_cache_dir(
            bundle_path,
            "sha256:a1b2c3d4e5f6789012345678901234567890abcd",
        );

        // Bundle content changed - new manifest digest
        let cache_v2 = layout.local_bundle_cache_dir(
            bundle_path,
            "sha256:f9e8d7c6b5a4321098765432109876543210fedc",
        );

        // CRITICAL: Different manifest = different cache dir
        // This ensures stale cache is never used after content change
        assert_ne!(
            cache_v1, cache_v2,
            "Same path but different manifest should use DIFFERENT cache dirs"
        );

        // Both should be under the same parent (local/)
        assert_eq!(cache_v1.parent(), cache_v2.parent());

        // Verify the cache dir names differ in the manifest portion
        let name_v1 = cache_v1.file_name().unwrap().to_str().unwrap();
        let name_v2 = cache_v2.file_name().unwrap().to_str().unwrap();

        // Same path hash (first part), different manifest (second part)
        let parts_v1: Vec<&str> = name_v1.split('-').collect();
        let parts_v2: Vec<&str> = name_v2.split('-').collect();

        assert_eq!(
            parts_v1[0], parts_v2[0],
            "Same path should have same path hash"
        );
        assert_ne!(
            parts_v1[1], parts_v2[1],
            "Different manifest should have different hash"
        );
    }

    #[test]
    fn test_local_bundle_cache_same_content_same_cache() {
        // Verify idempotency: same inputs = same cache dir
        let layout = ImageFilesystemLayout::new(PathBuf::from("/images"));

        let cache1 = layout.local_bundle_cache_dir(Path::new("/my/bundle"), "sha256:abc123");
        let cache2 = layout.local_bundle_cache_dir(Path::new("/my/bundle"), "sha256:abc123");

        assert_eq!(
            cache1, cache2,
            "Same path + manifest should give same cache dir"
        );
    }

    #[test]
    fn test_local_bundle_different_paths_different_caches() {
        // Different bundle paths should have different caches even with same manifest
        let layout = ImageFilesystemLayout::new(PathBuf::from("/images"));
        let manifest = "sha256:same_manifest";

        let cache1 = layout.local_bundle_cache_dir(Path::new("/bundle1"), manifest);
        let cache2 = layout.local_bundle_cache_dir(Path::new("/bundle2"), manifest);

        assert_ne!(
            cache1, cache2,
            "Different paths should have different cache dirs"
        );
    }

    #[test]
    fn test_different_boxes_get_different_net_backend_socket_paths() {
        // Regression test for gvproxy socket collision bug.
        // OLD CODE: Socket paths were generated inside Go as /tmp/gvproxy-{id}.sock
        //           with id starting at 1 per process — two shim processes would both
        //           create /tmp/gvproxy-1.sock, causing a collision.
        // NEW CODE: Each box gets its own socket path from the layout.

        let config = FsLayoutConfig::without_bind_mount();
        let box_a = BoxFilesystemLayout::new(
            PathBuf::from("/home/user/.boxlite/boxes/box-aaa"),
            config.clone(),
            false,
        );
        let box_b = BoxFilesystemLayout::new(
            PathBuf::from("/home/user/.boxlite/boxes/box-bbb"),
            config,
            false,
        );

        let path_a = box_a.net_backend_socket_path();
        let path_b = box_b.net_backend_socket_path();

        // CRITICAL: Different boxes MUST have different socket paths
        // This was the root cause of the collision bug
        assert_ne!(
            path_a, path_b,
            "Two different boxes must have different net backend socket paths"
        );

        // Binding paths live under the per-box /tmp symlink; the real dirs
        // stay distinct per box too.
        assert!(path_a.parent().unwrap().ends_with("box-aaa"));
        assert!(path_b.parent().unwrap().ends_with("box-bbb"));
        assert_ne!(box_a.sockets_dir(), box_b.sockets_dir());

        // Verify the socket filename
        assert_eq!(path_a.file_name().unwrap(), "net.sock");
        assert_eq!(path_b.file_name().unwrap(), "net.sock");
    }

    #[test]
    fn test_long_home_routes_sockets_through_binding_symlink() {
        // A home deep enough that sockets/net.sock-krun.sock would exceed the
        // sun_path limit. prepare() must create the binding symlink and the
        // socket getters must bind through it; sockets_dir() stays real.
        let tmp = tempfile::TempDir::new().unwrap();
        let deep_boxes = tmp
            .path()
            .join("a_very_long_segment_to_push_paths_over_the_limit")
            .join("and_one_more_for_good_measure")
            .join("boxes");
        let box_dir = deep_boxes.join("sunpathbox1");
        let layout =
            BoxFilesystemLayout::new(box_dir.clone(), FsLayoutConfig::without_bind_mount(), false);
        assert!(
            layout
                .sockets_dir()
                .join("net.sock-krun.sock")
                .as_os_str()
                .len()
                >= 104,
            "test setup must exceed sun_path"
        );

        layout.prepare().unwrap();

        let binding_dir = layout.sockets().binding_dir();
        let bound = layout.net_backend_socket_path();
        assert!(
            bound.starts_with(&binding_dir),
            "expected binding path under {}, got {}",
            binding_dir.display(),
            bound.display()
        );
        assert!(bound.as_os_str().len() + "-krun.sock".len() < 104);
        assert_eq!(layout.sockets_dir(), box_dir.join("sockets"));
        // The symlink resolves to the real sockets dir.
        assert_eq!(
            std::fs::read_link(&binding_dir).unwrap(),
            layout.sockets_dir()
        );

        // cleanup() removes the symlink together with the box dir.
        layout.cleanup().unwrap();
        assert!(std::fs::symlink_metadata(&binding_dir).is_err());
    }

    #[test]
    fn test_bases_dir() {
        let layout = FilesystemLayout::new(
            PathBuf::from("/home/user/.boxlite"),
            FsLayoutConfig::without_bind_mount(),
        );
        assert_eq!(
            layout.bases_dir(),
            PathBuf::from("/home/user/.boxlite/bases")
        );
    }

    #[test]
    fn test_prepare_creates_bases_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let layout = FilesystemLayout::new(
            dir.path().to_path_buf(),
            FsLayoutConfig::without_bind_mount(),
        );
        layout.prepare().unwrap();

        assert!(layout.bases_dir().exists());
        assert!(layout.temp_dir().exists());
        assert!(layout.boxes_dir().exists());
    }

    #[test]
    fn test_prepare_validates_same_filesystem() {
        let dir = tempfile::TempDir::new().unwrap();
        let layout = FilesystemLayout::new(
            dir.path().to_path_buf(),
            FsLayoutConfig::without_bind_mount(),
        );
        // All dirs under same temp dir → same filesystem → should pass
        layout.prepare().unwrap();

        // Verify the validation passes independently
        layout.validate_same_filesystem().unwrap();
    }

    // ========================================================================
    // BoxFilesystemLayout — jailer path method tests
    // ========================================================================

    fn test_box_layout(box_dir: &str) -> BoxFilesystemLayout {
        BoxFilesystemLayout::new(
            PathBuf::from(box_dir),
            FsLayoutConfig::without_bind_mount(),
            false,
        )
    }

    #[test]
    fn test_box_layout_logs_dir() {
        let layout = test_box_layout("/home/.boxlite/boxes/mybox");
        assert_eq!(
            layout.logs_dir(),
            PathBuf::from("/home/.boxlite/boxes/mybox/logs")
        );
    }

    #[test]
    fn test_box_layout_bin_dir() {
        let layout = test_box_layout("/home/.boxlite/boxes/mybox");
        assert_eq!(
            layout.bin_dir(),
            PathBuf::from("/home/.boxlite/boxes/mybox/bin")
        );
    }

    #[test]
    fn test_box_layout_boot_dir() {
        let layout = test_box_layout("/home/.boxlite/boxes/mybox");
        assert_eq!(
            layout.boot_dir(),
            PathBuf::from("/home/.boxlite/boxes/mybox/boot")
        );
    }

    #[test]
    fn test_box_layout_tmp_dir() {
        let layout = test_box_layout("/home/.boxlite/boxes/mybox");
        assert_eq!(
            layout.tmp_dir(),
            PathBuf::from("/home/.boxlite/boxes/mybox/tmp")
        );
    }

    #[test]
    fn test_box_layout_guest_rootfs_disk_path() {
        let layout = test_box_layout("/home/.boxlite/boxes/mybox");
        assert_eq!(
            layout.guest_rootfs_disk_path(),
            PathBuf::from("/home/.boxlite/boxes/mybox/disks/guest-rootfs.qcow2")
        );
    }

    #[test]
    fn test_box_layout_disks_dir() {
        let layout = test_box_layout("/home/.boxlite/boxes/mybox");
        assert_eq!(
            layout.disks_dir(),
            PathBuf::from("/home/.boxlite/boxes/mybox/disks")
        );
    }

    #[test]
    fn test_box_layout_disk_path() {
        let layout = test_box_layout("/home/.boxlite/boxes/mybox");
        assert_eq!(
            layout.disk_path(),
            PathBuf::from("/home/.boxlite/boxes/mybox/disks/disk.qcow2")
        );
    }

    /// All jailer-relevant paths must be rooted under box_dir.
    /// This is the core guarantee: the sandbox never grants access outside the box.
    #[test]
    fn test_box_layout_all_jailer_paths_inside_box_dir() {
        let box_dir = "/home/.boxlite/boxes/test";
        let layout = test_box_layout(box_dir);

        let paths = [
            layout.logs_dir(),
            layout.bin_dir(),
            layout.boot_dir(),
            layout.sockets_dir(),
            layout.tmp_dir(),
            layout.guest_rootfs_disk_path(),
            layout.exit_file_path(),
            layout.console_output_path(),
            layout.disk_path(),
        ];

        for path in &paths {
            assert!(
                path.starts_with(box_dir),
                "Path {} should be inside box_dir {}",
                path.display(),
                box_dir
            );
        }
    }

    #[test]
    fn test_box_layout_prepare_creates_tmp_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let box_dir = dir.path().join("box-tmp-test");
        let layout = BoxFilesystemLayout::new(box_dir, FsLayoutConfig::without_bind_mount(), false);

        layout.prepare().unwrap();

        assert!(layout.tmp_dir().exists());
    }
}
