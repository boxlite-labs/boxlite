use crate::db::{BoxStore, Database};
use crate::experimental::ExperimentalFeatures;
use crate::images::{ImageDiskManager, ImageManager};
use crate::litebox::config::BoxConfig;
use crate::litebox::{BoxManager, LiteBox, LocalSnapshotBackend, SharedBoxImpl};
use crate::lock::{FileLockManager, LockManager};
use crate::metrics::{RuntimeMetrics, RuntimeMetricsStorage};
use crate::rootfs::guest::{GuestRootfs, GuestRootfsManager};
use crate::runtime::id::{BoxID, BoxIDMint};
use crate::runtime::layout::{BoxFilesystemLayout, FilesystemLayout, FsLayoutConfig};
use crate::runtime::lock::RuntimeLock;
use crate::runtime::options::{BoxArchive, BoxOptions, BoxliteOptions};
use crate::runtime::signal_handler::timeout_to_duration;
use crate::runtime::types::{BoxInfo, BoxState, BoxStatus, ContainerID};
use crate::vmm::VmmKind;
use crate::vmm::controller::{ShimHandler, VmmHandler};
use boxlite_shared::{BoxliteError, BoxliteResult};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, RwLock, Weak};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

fn litebox_from_impl(box_impl: SharedBoxImpl) -> LiteBox {
    // Every handle this runtime hands out starts following its box's main
    // command, if the box is running and nobody is following it yet.
    //
    // `start()` arms the watcher for boxes *we* start. This covers the ones we
    // merely adopt — recovered from a previous process, already Running with a
    // live shim. That is what a long-lived runtime meets on every restart
    // (`boxlite serve`, the cloud), and without it such a box would run to
    // completion entirely unobserved and be reported Running forever: exactly
    // the lie the watcher exists to stop telling.
    box_impl.arm_watcher(None);

    let box_backend: Arc<dyn crate::runtime::backend::BoxBackend> = box_impl.clone();
    let network_backend: Arc<dyn crate::runtime::backend::BoxNetworkBackend> = box_impl.clone();
    let snapshot_backend: Arc<dyn crate::runtime::backend::SnapshotBackend> =
        Arc::new(LocalSnapshotBackend::new(box_impl));
    LiteBox::new(box_backend, network_backend, snapshot_backend)
}

/// Record that the box's main command is over, taking its exit code from the
/// guest-written exit file.
///
/// Two callers reach this from opposite directions and must agree: the live
/// watcher in `BoxImpl` (a long-lived runtime sees the shim die) and
/// `recover_boxes` (a fresh process finds a Running box with a dead shim).
/// A missing record leaves `exit_code` as None rather than inventing a 0 —
/// the box stopped, but nobody witnessed how.
pub(crate) fn record_main_command_exit(state: &mut BoxState, exit_file: &std::path::Path) {
    if let Some(record) = boxlite_shared::layout::ExitRecord::read(exit_file) {
        state.exit_code = Some(record.exit_code);
    }
    state.mark_stop();
}

/// Archive the active exit file as `exit.previous`, freeing the canonical
/// slot for the next crash. Single-slot rotation — any prior `exit.previous`
/// is overwritten. NotFound on source is tolerated.
pub(crate) fn stash_exit_file(layout: &BoxFilesystemLayout) {
    let src = layout.exit_file_path();
    let dst = layout.exit_previous_path();
    match std::fs::rename(&src, &dst) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            error = %e,
            src = %src.display(),
            "Failed to stash exit file"
        ),
    }
}

/// Move a directory tree, falling back to recursive copy + remove if rename
/// fails with EXDEV. The import/clone staging dir and the canonical boxes dir
/// can live on different filesystems (e.g. a tmpfs scratch area vs the on-disk
/// data dir), where a plain `rename()` fails with "Invalid cross-device link".
fn move_dir_cross_fs(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_dir_recursive(src, dst)?;
            std::fs::remove_dir_all(src)
        }
        Err(e) => Err(e),
    }
}

/// Recursively copy a directory tree, preserving symlinks as symlinks.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if ty.is_symlink() {
            let target = std::fs::read_link(&from)?;
            std::os::unix::fs::symlink(target, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Internal runtime state protected by single lock.
///
/// **Shared via Arc**: This is the actual shared state that can be cloned cheaply.
pub type SharedRuntimeImpl = Arc<RuntimeImpl>;

/// Runtime inner implementation.
///
/// **Locking Strategy**:
/// - `sync_state`: Empty coordination lock - acquire when multi-step operations
///   on box_manager/image_manager need atomicity
/// - All managers have internal locking for individual operations
/// - Immutable fields: No lock needed - never change after creation
/// - Atomic fields: Lock-free (RuntimeMetricsStorage uses AtomicU64)
pub struct RuntimeImpl {
    /// Coordination lock for multi-step atomic operations.
    /// Acquire this BEFORE accessing box_manager/image_manager
    /// when you need atomicity across multiple operations.
    pub(crate) sync_state: RwLock<SynchronizedState>,

    // ========================================================================
    // COORDINATION REQUIRED: Acquire sync_state lock for multi-step operations
    // ========================================================================
    /// Box manager with integrated persistence (has internal RwLock)
    pub(crate) box_manager: BoxManager,
    /// Image management (has internal RwLock via ImageStore)
    pub(crate) image_manager: ImageManager,

    // ========================================================================
    // NO COORDINATION NEEDED: Immutable or internally synchronized
    // ========================================================================
    /// Filesystem layout (immutable after init)
    pub(crate) layout: FilesystemLayout,
    /// Pure image disk cache manager (image layers → ext4, no guest binary)
    pub(crate) image_disk_mgr: ImageDiskManager,
    /// Versioned guest rootfs manager (image disk + guest binary → ext4)
    pub(crate) guest_rootfs_mgr: GuestRootfsManager,
    /// Guest rootfs lazy initialization (Arc<OnceCell>)
    pub(crate) guest_rootfs: Arc<OnceCell<GuestRootfs>>,
    /// Runtime-wide metrics (AtomicU64 based, lock-free)
    pub(crate) runtime_metrics: RuntimeMetricsStorage,

    /// Immutable release-candidate capability opt-ins for this runtime.
    pub(crate) experimental_features: ExperimentalFeatures,

    /// Base disk manager for clone base lifecycle and ref-count tracking.
    pub(crate) base_disk_mgr: crate::disk::BaseDiskManager,

    /// Snapshot manager for per-box snapshot lifecycle (create, remove, restore).
    pub(crate) snapshot_mgr: crate::litebox::snapshot_mgr::SnapshotManager,

    /// Per-entity lock manager for multiprocess-safe locking.
    ///
    /// Provides locks for individual entities (boxes, volumes, etc.) that work
    /// across multiple processes. Similar to Podman's lock manager.
    pub(crate) lock_manager: Arc<dyn LockManager>,

    /// Abstract factory for boxes' network control backends (the one place a
    /// concrete backend is chosen). Boxes create their backend through this, so
    /// no call site names a concrete backend — see [`crate::net::NetworkBackendFactory`].
    pub(crate) network_factory: Arc<dyn crate::net::NetworkBackendFactory>,

    /// Runtime filesystem lock (held for lifetime). Prevent from multiple process run on same
    /// BOXLITE_HOME directory
    pub(crate) _runtime_lock: RuntimeLock,

    // ========================================================================
    // SHUTDOWN COORDINATION
    // ========================================================================
    /// Cancellation token for coordinated shutdown.
    /// When cancelled, all in-flight operations should terminate gracefully.
    /// Use `.is_cancelled()` for sync checks, `.cancelled()` for async select!.
    /// Child tokens are passed to each box via `.child_token()`.
    pub(crate) shutdown_token: CancellationToken,
}

/// Synchronized state protected by RwLock.
///
/// Acquire this when you need atomicity across multiple operations on
/// box_manager or image_manager.
pub struct SynchronizedState {
    /// Cache of active BoxImpl instances by ID.
    /// Uses Weak to allow automatic cleanup when all handles are dropped.
    active_boxes_by_id: HashMap<BoxID, Weak<crate::litebox::box_impl::BoxImpl>>,
    /// Cache of active BoxImpl instances by name (only for named boxes).
    active_boxes_by_name: HashMap<String, Weak<crate::litebox::box_impl::BoxImpl>>,
}

/// Headroom `ImageDiskManager` reserves in every image disk it builds, for
/// the `boxlite-guest` binary `GuestRootfsManager` injects into a copy of it
/// afterward. `ImageDiskManager` folds this into its cache key
/// (`disk_path`), so a disk cached under a different value is never reused.
///
/// Fixed, not derived from the real guest binary's current size: this
/// manager has no GC (unlike `GuestRootfsManager`'s paired `version_key` +
/// `gc()`), so a value that changed on every guest-binary rebuild would
/// orphan a cache entry per digest on every rebuild — a routine, frequent
/// event for developers. Because this is instead a source-level constant, it
/// only changes on the rare, deliberate occasions *this number itself* gets
/// edited (e.g. if a future guest binary outgrows it), each such change
/// creating at most one new generation of cache entries, not a continuous
/// leak. An unstripped debug build measures ~232 MB (see
/// `guest_binary_fits_in_image_disk_headroom` below); 512 MiB covers that
/// with margin to grow, in both debug and (far smaller) release builds, at
/// the cost of some unused — but sparse, so cheap on disk — space in every
/// cached image disk.
const IMAGE_DISK_GUEST_BINARY_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;

impl RuntimeImpl {
    // ========================================================================
    // CONSTRUCTION
    // ========================================================================

    /// Create a new RuntimeInnerImpl with the provided options.
    ///
    /// Performs all initialization: filesystem setup, locks, managers, and box recovery.
    pub fn new(options: BoxliteOptions) -> BoxliteResult<SharedRuntimeImpl> {
        Self::new_with_experimental_features(options, ExperimentalFeatures::default())
    }

    pub(crate) fn new_with_experimental_features(
        options: BoxliteOptions,
        experimental_features: ExperimentalFeatures,
    ) -> BoxliteResult<SharedRuntimeImpl> {
        let _sys = crate::system_check::SystemCheck::run()?;
        Self::initialize(options, experimental_features)
    }

    /// Build a runtime without host validation. Tests using this must not exercise
    /// VM or hypervisor operations.
    #[cfg(test)]
    pub(crate) fn new_for_test(options: BoxliteOptions) -> BoxliteResult<SharedRuntimeImpl> {
        Self::initialize(options, ExperimentalFeatures::default())
    }

    fn initialize(
        options: BoxliteOptions,
        experimental_features: ExperimentalFeatures,
    ) -> BoxliteResult<SharedRuntimeImpl> {
        // Validate Early: Check preconditions before expensive work
        if !options.home_dir.is_absolute() {
            return Err(BoxliteError::Internal(format!(
                "home_dir must be absolute path, got: {}",
                options.home_dir.display()
            )));
        }

        // Configure bind mount support based on platform
        #[cfg(target_os = "linux")]
        let fs_config = FsLayoutConfig::with_bind_mount();
        #[cfg(not(target_os = "linux"))]
        let fs_config = FsLayoutConfig::without_bind_mount();

        let layout = FilesystemLayout::new(options.home_dir.clone(), fs_config);

        layout.prepare().map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to initialize filesystem at {}: {}",
                layout.home_dir().display(),
                e
            ))
        })?;

        let runtime_lock = RuntimeLock::acquire(layout.home_dir()).map_err(|e| {
            BoxliteError::Internal(format!(
                "Failed to acquire runtime lock at {}: {}",
                layout.home_dir().display(),
                e
            ))
        })?;

        // Clean temp dir contents to avoid stale files from previous runs
        if let Ok(entries) = std::fs::read_dir(layout.temp_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        // Sweep socket binding symlinks whose boxes are gone (crash leftovers
        // that per-box removal never saw).
        crate::net::socket_path::BoxSockets::sweep_stale();

        let db = Database::open(&layout.db_dir().join("boxlite.db")).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to initialize database at {}: {}",
                layout.db_dir().join("boxlite.db").display(),
                e
            ))
        })?;

        let image_manager =
            ImageManager::new(layout.images_dir(), db.clone(), options.image_registries).map_err(
                |e| {
                    BoxliteError::Storage(format!(
                        "Failed to initialize image manager at {}: {}",
                        layout.images_dir().display(),
                        e
                    ))
                },
            )?;

        let base_disk_store = crate::db::BaseDiskStore::new(db.clone());
        let base_disk_mgr =
            crate::disk::BaseDiskManager::new(layout.bases_dir(), base_disk_store.clone());
        let snapshot_store = crate::db::SnapshotStore::new(db.clone());
        let snapshot_mgr = crate::litebox::snapshot_mgr::SnapshotManager::new(snapshot_store);
        let box_store = BoxStore::new(db);

        // Initialize lock manager for per-entity multiprocess-safe locking
        let lock_manager: Arc<dyn LockManager> =
            Arc::new(FileLockManager::new(layout.locks_dir()).map_err(|e| {
                BoxliteError::Storage(format!(
                    "Failed to initialize lock manager at {}: {}",
                    layout.locks_dir().display(),
                    e
                ))
            })?);

        tracing::debug!(
            lock_dir = %layout.locks_dir().display(),
            "Initialized lock manager"
        );

        // See IMAGE_DISK_GUEST_BINARY_HEADROOM_BYTES for why this is a fixed
        // budget rather than derived from the guest binary actually on disk.
        let image_disk_mgr = ImageDiskManager::new(
            layout.image_layout().disk_images_dir(),
            layout.temp_dir(),
            IMAGE_DISK_GUEST_BINARY_HEADROOM_BYTES,
        );
        let guest_rootfs_mgr = GuestRootfsManager::new(base_disk_mgr.clone(), layout.temp_dir());

        let inner = Arc::new(Self {
            sync_state: RwLock::new(SynchronizedState {
                active_boxes_by_id: HashMap::new(),
                active_boxes_by_name: HashMap::new(),
            }),
            box_manager: BoxManager::new(box_store),
            image_manager,
            layout,
            image_disk_mgr,
            guest_rootfs_mgr,
            guest_rootfs: Arc::new(OnceCell::new()),
            runtime_metrics: RuntimeMetricsStorage::new(),
            experimental_features,
            base_disk_mgr,
            snapshot_mgr,
            lock_manager,
            network_factory: crate::net::default_factory(),
            _runtime_lock: runtime_lock,
            shutdown_token: CancellationToken::new(),
        });

        tracing::debug!("initialized runtime");

        // Recover boxes from database
        inner.recover_boxes()?;

        Ok(inner)
    }

    // ========================================================================
    // PUBLIC API - BOX OPERATIONS
    // ========================================================================

    /// Create a box handle.
    ///
    /// Allocates lock, persists to database with Configured status, and returns
    /// a LiteBox handle. The VM is not started until start() or exec() is called.
    ///
    /// This method is async for API consistency with other runtime methods.
    pub async fn create(
        self: &Arc<Self>,
        options: BoxOptions,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        let (litebox, _created) = self.create_inner(options, name, false).await?;
        Ok(litebox)
    }

    /// Get an existing box by name, or create a new one if it doesn't exist.
    ///
    /// Returns `(LiteBox, true)` if a new box was created, or `(LiteBox, false)`
    /// if an existing box with the given name was found. When an existing box is
    /// returned, general options are ignored, but its capability policy must
    /// match exactly and it must already provide any required nested
    /// virtualization, so reuse cannot silently weaken privileges or drop a
    /// requested capability.
    pub async fn get_or_create(
        self: &Arc<Self>,
        options: BoxOptions,
        name: Option<String>,
    ) -> BoxliteResult<(LiteBox, bool)> {
        self.create_inner(options, name, true).await
    }

    /// Import a box from a `.boxlite` archive.
    ///
    /// Creates a new box with a new ID from archived disk images and
    /// configuration. The imported box starts in `Stopped` state.
    pub async fn import_box(
        self: &Arc<Self>,
        archive: BoxArchive,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        if self.shutdown_token.is_cancelled() {
            return Err(BoxliteError::Stopped(
                "Cannot import box: runtime has been shut down".into(),
            ));
        }
        super::import::import_box(self, archive, name).await
    }

    /// Inner create logic shared by `create()` and `get_or_create()`.
    ///
    /// When `reuse_existing` is false, returns an error if a box with the same
    /// name already exists (standard create behavior). When true, returns the
    /// existing box with `created=false`.
    async fn create_inner(
        self: &Arc<Self>,
        options: BoxOptions,
        name: Option<String>,
        reuse_existing: bool,
    ) -> BoxliteResult<(LiteBox, bool)> {
        // Check if runtime has been shut down
        if self.shutdown_token.is_cancelled() {
            return Err(BoxliteError::Stopped(
                "Cannot create box: runtime has been shut down".into(),
            ));
        }

        // Check DB for existing name — use lookup_box to get full (config, state)
        // so we can build the LiteBox directly without a second lookup
        if let Some(ref name) = name
            && let Some((config, state)) = self.box_manager.lookup_box(name)?
        {
            return if reuse_existing {
                self.adopt_existing_box(&options, config, state)
            } else {
                Err(BoxliteError::InvalidArgument(format!(
                    "box with name '{}' already exists",
                    name
                )))
            };
        }

        let store = crate::volumes::LocalNamedVolumeStore::new(self.layout.home_dir());
        let options = tokio::task::spawn_blocking(move || {
            let mut options = options;
            resolve_managed_volumes(&store, &mut options)?;
            Ok::<_, BoxliteError>(options)
        })
        .await
        .map_err(|e| {
            BoxliteError::Internal(format!("failed to join volume creation task: {e}"))
        })??;

        // Initialize box variables with defaults
        let (config, mut state) = self.init_box_variables(&options, name.clone());

        // Allocate lock for this box
        let lock_id = self.lock_manager.allocate()?;
        state.set_lock_id(lock_id);

        // Persist to database immediately (status = Configured)
        if let Err(e) = self.box_manager.add_box(&config, &state) {
            // Clean up the allocated lock on failure
            if let Err(free_err) = self.lock_manager.free(lock_id) {
                tracing::error!(
                    lock_id = %lock_id,
                    error = %free_err,
                    "Failed to free lock after DB persist error"
                );
            }

            // TOCTOU race recovery: lookup_box (line ~268) and add_box are
            // separate non-atomic operations. Between them, another concurrent
            // caller can complete the full create path and persist first:
            //
            //   Task A: lookup("w") → None     Task B: lookup("w") → None
            //   Task A: add_box() → Ok         Task B: add_box() → Err (duplicate)
            //
            // When reuse_existing=true, recover by re-reading the winner's box.
            if reuse_existing
                && let Some(ref name) = name
                && let Some((config, state)) = self.box_manager.lookup_box(name)?
            {
                return self.adopt_existing_box(&options, config, state);
            }

            return Err(e);
        }

        tracing::debug!(
            box_id = %config.id,
            lock_id = %lock_id,
            "Created box with Configured status"
        );

        // Create LiteBox handle with shared BoxImpl
        // This also checks in-memory cache for duplicate names
        let (box_impl, inserted) = self.get_or_create_box_impl(config, state);
        if !inserted {
            return Err(BoxliteError::InvalidArgument(
                "box with this name already exists".into(),
            ));
        }

        // Increment boxes_created counter (lock-free!)
        self.runtime_metrics
            .boxes_created
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok((litebox_from_impl(box_impl), true))
    }

    /// Adopt a box found either by the initial lookup or duplicate-create recovery.
    fn adopt_existing_box(
        self: &Arc<Self>,
        requested: &BoxOptions,
        config: BoxConfig,
        state: BoxState,
    ) -> BoxliteResult<(LiteBox, bool)> {
        Self::check_options_compatibility(requested, &config)?;
        let (box_impl, _) = self.get_or_create_box_impl(config, state);
        Ok((litebox_from_impl(box_impl), false))
    }

    /// Reject reuse when the request disagrees with the box's stored options.
    ///
    /// Only the capability policy is compared: silently adopting a box whose
    /// privileges differ from the request is the case that matters for safety.
    fn check_options_compatibility(
        requested: &BoxOptions,
        actual: &BoxConfig,
    ) -> BoxliteResult<()> {
        let box_name = actual.name.as_deref().unwrap_or_else(|| actual.id.as_str());

        // One-way: a nested-capable box satisfies any request, but a plain box
        // cannot satisfy one that needs /dev/kvm. Reusing it would look like a
        // success and defer the failure into the guest.
        if requested.advanced.nested_virtualization
            && !actual.options.advanced.nested_virtualization
        {
            return Err(BoxliteError::Unsupported(format!(
                "box '{box_name}' was created without nested virtualization and cannot satisfy a required nested virtualization request; use a different name or recreate the box"
            )));
        }
        if requested.advanced.privileged && !actual.options.advanced.privileged {
            return Err(BoxliteError::Unsupported(format!(
                "box '{box_name}' was created without privileged support and cannot satisfy a required privileged request; use a different name or recreate the box"
            )));
        }
        // The mirror case: privileged governs mount hardening (readonly
        // paths, /sys write access) as well as capabilities, so a privileged
        // box can look capability-compatible with a non-privileged request
        // (e.g. one that explicitly asks for `capabilities.add = ["ALL"]`)
        // while still granting more than that request asked for. Reject
        // instead of silently upgrading it.
        if !requested.advanced.privileged && actual.options.advanced.privileged {
            return Err(BoxliteError::Unsupported(format!(
                "box '{box_name}' was created with privileged mode and would grant more than the requested non-privileged security policy; use a different name or recreate the box"
            )));
        }

        // Compare effective capabilities, not the raw field: a privileged
        // box persisted by an earlier version of this option has `add=["ALL"]`
        // mutated into its stored capabilities, while a new privileged
        // request idiomatically leaves capabilities empty — both resolve to
        // the same thing.
        requested
            .advanced
            .effective_capabilities()
            .check_compatibility(&actual.options.advanced.effective_capabilities(), box_name)?;

        Ok(())
    }

    /// Get a handle to an existing box by ID or name.
    ///
    /// Returns a LiteBox handle that can be used to operate on the box.
    /// Checks in-memory cache first (for boxes not yet persisted), then DB.
    ///
    /// If another handle to the same box exists, they share the same BoxImpl
    /// (and thus the same LiveState if initialized).
    pub async fn get(self: &Arc<Self>, id_or_name: &str) -> BoxliteResult<Option<LiteBox>> {
        tracing::trace!(id_or_name = %id_or_name, "RuntimeInnerImpl::get called");

        // Check in-memory cache first (for boxes created but not yet persisted)
        {
            let sync = self.sync_state.read().unwrap();

            // Try as BoxID first
            if let Some(box_id) = BoxID::parse(id_or_name)
                && let Some(weak) = sync.active_boxes_by_id.get(&box_id)
                && let Some(strong) = weak.upgrade()
            {
                tracing::trace!(box_id = %box_id, "Found box in cache by ID");
                return Ok(Some(litebox_from_impl(strong)));
            }

            // Try as name
            if let Some(weak) = sync.active_boxes_by_name.get(id_or_name)
                && let Some(strong) = weak.upgrade()
            {
                tracing::trace!(name = %id_or_name, "Found box in cache by name");
                return Ok(Some(litebox_from_impl(strong)));
            }
        }

        // Fall back to DB lookup (for persisted boxes) - run on blocking thread pool
        let this = Arc::clone(self);
        let id_or_name_owned = id_or_name.to_string();
        let db_result =
            tokio::task::spawn_blocking(move || this.box_manager.lookup_box(&id_or_name_owned))
                .await
                .map_err(|e| BoxliteError::Internal(format!("spawn_blocking failed: {}", e)))??;

        if let Some((config, state)) = db_result {
            tracing::trace!(
                box_id = %config.id,
                name = ?config.name,
                "Retrieved box from DB, getting or creating BoxImpl"
            );

            let (box_impl, _) = self.get_or_create_box_impl(config, state);
            tracing::trace!(id_or_name = %id_or_name, "LiteBox created successfully");
            return Ok(Some(litebox_from_impl(box_impl)));
        }

        tracing::trace!(id_or_name = %id_or_name, "Box not found");
        Ok(None)
    }

    /// Remove a box completely by ID or name.
    pub fn remove(&self, id_or_name: &str, force: bool) -> BoxliteResult<()> {
        let box_id = self.resolve_id(id_or_name)?;
        self.remove_box(&box_id, force)
    }

    // ========================================================================
    // PUBLIC API - QUERY OPERATIONS
    // ========================================================================

    /// Get information about a specific box by ID or name (without creating a handle).
    ///
    /// Checks in-memory cache first (for boxes not yet persisted), then database.
    pub async fn get_info(self: &Arc<Self>, id_or_name: &str) -> BoxliteResult<Option<BoxInfo>> {
        // Check in-memory cache first (for boxes created but not yet persisted)
        let cached_box = {
            let sync = self.sync_state.read().unwrap();

            // Try as BoxID first
            if let Some(box_id) = BoxID::parse(id_or_name)
                && let Some(weak) = sync.active_boxes_by_id.get(&box_id)
                && let Some(strong) = weak.upgrade()
            {
                Some(strong)
            } else if let Some(weak) = sync.active_boxes_by_name.get(id_or_name) {
                weak.upgrade()
            } else {
                None
            }
        };

        if let Some(box_impl) = cached_box {
            return Ok(Some(box_impl.info()));
        }

        // Fall back to DB lookup - run on blocking thread pool
        let this = Arc::clone(self);
        let id_or_name_owned = id_or_name.to_string();
        let db_result =
            tokio::task::spawn_blocking(move || this.box_manager.lookup_box(&id_or_name_owned))
                .await
                .map_err(|e| BoxliteError::Internal(format!("spawn_blocking failed: {}", e)))??;

        Ok(db_result.map(|(config, state)| BoxInfo::new(&config, &state)))
    }

    /// List all boxes, sorted by creation time (newest first).
    ///
    /// Includes both persisted boxes (from database) and in-memory boxes
    /// (created but not yet persisted).
    pub async fn list_info(self: &Arc<Self>) -> BoxliteResult<Vec<BoxInfo>> {
        use std::collections::HashSet;

        // Get boxes from database - run on blocking thread pool
        let this = Arc::clone(self);
        let db_boxes = tokio::task::spawn_blocking(move || this.box_manager.all_boxes(true))
            .await
            .map_err(|e| BoxliteError::Internal(format!("spawn_blocking failed: {}", e)))??;

        let mut seen_ids = HashSet::with_capacity(db_boxes.len());
        let mut infos = Vec::with_capacity(db_boxes.len());
        for (config, state) in db_boxes {
            seen_ids.insert(config.id.clone());
            // Database rows are the cross-process freshness authority. Build
            // their metadata directly instead of manufacturing live handles.
            infos.push(BoxInfo::new(&config, &state));
        }

        // Add in-memory boxes not yet persisted. `info()` reads the box's
        // shim.pid, so collect the handles first rather than doing that file
        // I/O while holding the cache lock.
        let in_memory_only: Vec<_> = {
            let sync = self.sync_state.read().unwrap();
            sync.active_boxes_by_id
                .iter()
                .filter(|(box_id, _)| !seen_ids.contains(*box_id))
                .filter_map(|(_, weak)| weak.upgrade())
                .collect()
        };
        infos.extend(in_memory_only.iter().map(|box_impl| box_impl.info()));

        // Sort by creation time (newest first)
        infos.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(infos)
    }

    /// Check if a box with the given ID or name exists.
    ///
    /// Checks in-memory cache first (for boxes not yet persisted), then database.
    pub async fn exists(self: &Arc<Self>, id_or_name: &str) -> BoxliteResult<bool> {
        // Check in-memory cache first
        {
            let sync = self.sync_state.read().unwrap();

            // Try as BoxID first
            if let Some(box_id) = BoxID::parse(id_or_name)
                && let Some(weak) = sync.active_boxes_by_id.get(&box_id)
                && weak.upgrade().is_some()
            {
                return Ok(true);
            }

            // Try as name
            if let Some(weak) = sync.active_boxes_by_name.get(id_or_name)
                && weak.upgrade().is_some()
            {
                return Ok(true);
            }
        }

        // Fall back to DB lookup - run on blocking thread pool
        let this = Arc::clone(self);
        let id_or_name_owned = id_or_name.to_string();
        let db_result =
            tokio::task::spawn_blocking(move || this.box_manager.lookup_box_id(&id_or_name_owned))
                .await
                .map_err(|e| BoxliteError::Internal(format!("spawn_blocking failed: {}", e)))??;

        Ok(db_result.is_some())
    }

    // ========================================================================
    // PUBLIC API - METRICS
    // ========================================================================

    /// Get runtime-wide metrics.
    pub async fn metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics::new(self.runtime_metrics.clone())
    }

    // ========================================================================
    // PUBLIC API - SHUTDOWN
    // ========================================================================

    /// Gracefully shutdown all non-detached boxes in this runtime.
    ///
    /// This method:
    /// 1. Marks the runtime as shut down (no new operations allowed)
    /// 2. Cancels the shutdown token (signals in-flight operations)
    /// 3. Stops all active non-detached boxes with the given timeout
    ///
    /// Detached boxes (`detach=true`) are skipped — they are designed to
    /// survive parent process exit and runtime shutdown.
    ///
    /// # Arguments
    /// * `timeout` - Seconds before force-kill. None=10s, Some(-1)=infinite
    ///
    /// # Returns
    /// Ok(()) if all boxes stopped successfully, Err if any box failed to stop.
    pub async fn shutdown(&self, timeout: Option<i32>) -> BoxliteResult<()> {
        // Check if already shut down (idempotent)
        if self.shutdown_token.is_cancelled() {
            return Ok(());
        }

        tracing::info!("Initiating runtime shutdown");

        // Cancel the shutdown token - marks shutdown and signals all in-flight operations
        self.shutdown_token.cancel();

        // Collect all active non-detached boxes
        let active_boxes: Vec<SharedBoxImpl> = {
            let sync = self.sync_state.read().unwrap();
            sync.active_boxes_by_id
                .values()
                .filter_map(|weak| weak.upgrade())
                .filter(|box_impl| !box_impl.config.options.detach)
                .collect()
        };

        if active_boxes.is_empty() {
            tracing::info!("No active boxes to shutdown");
            return Ok(());
        }

        tracing::info!(count = active_boxes.len(), "Stopping active boxes");

        // Convert timeout to duration
        let timeout_duration = timeout_to_duration(timeout);

        // Stop all boxes concurrently
        let stop_futures = active_boxes.iter().map(|box_impl| {
            let box_id = box_impl.id().to_string();
            async move {
                let result = if let Some(duration) = timeout_duration {
                    tokio::time::timeout(duration, box_impl.stop()).await
                } else {
                    // Infinite timeout
                    Ok(box_impl.stop().await)
                };
                (box_id, result)
            }
        });

        let results = futures::future::join_all(stop_futures).await;

        // Check for errors
        let mut errors = Vec::new();
        for (box_id, result) in results {
            match result {
                Ok(Ok(())) => {
                    tracing::debug!(box_id = %box_id, "Box stopped gracefully");
                }
                Ok(Err(e)) => {
                    tracing::warn!(box_id = %box_id, error = %e, "Box stop failed");
                    errors.push(format!("{}: {}", box_id, e));
                }
                Err(_) => {
                    tracing::warn!(box_id = %box_id, "Box stop timed out");
                    errors.push(format!("{}: timeout", box_id));
                }
            }
        }

        if errors.is_empty() {
            tracing::info!("Runtime shutdown complete");
            Ok(())
        } else {
            Err(BoxliteError::Internal(format!(
                "Shutdown completed with errors: {}",
                errors.join(", ")
            )))
        }
    }

    /// Synchronous shutdown for atexit/Drop contexts.
    ///
    /// At atexit/Drop time, all `LiteBox` handles are gone (Weak refs dead),
    /// so async `shutdown()` would find nothing. This method queries the DB
    /// directly and sends SIGTERM to shim processes. The shim's SIGTERM handler
    /// does graceful Guest.Shutdown() RPC (qcow2 flush) before exiting.
    ///
    /// Detached boxes are skipped (same contract as async `shutdown()`).
    pub(crate) fn shutdown_sync(&self) {
        if self.shutdown_token.is_cancelled() {
            return;
        }
        self.shutdown_token.cancel();

        let boxes = match self.box_manager.all_boxes(true) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[boxlite] Failed to query boxes during sync shutdown: {e}");
                return;
            }
        };

        for (config, mut state) in boxes {
            if state.status != BoxStatus::Running || config.options.detach {
                continue;
            }
            let Some(pid) = state.pid else { continue };
            if !crate::util::is_process_alive(pid) {
                continue;
            }

            eprintln!(
                "[boxlite] Auto-stopping non-detached box: id={}, pid={pid}",
                config.id
            );

            // SIGTERM triggers shim's graceful shutdown handler (Guest.Shutdown RPC)
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }

            // Wait for shim to finish graceful shutdown (3s guest RPC + margin)
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(5);
            loop {
                if !crate::util::is_process_alive(pid) {
                    break;
                }
                if start.elapsed() > timeout {
                    eprintln!(
                        "[boxlite] Shim didn't exit after SIGTERM, force killing: id={}, pid={pid}",
                        config.id
                    );
                    crate::util::kill_process(pid);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            let box_layout = self
                .layout
                .box_layout(config.id.as_str(), false)
                .expect("box_layout is infallible");

            // The third way a box dies, and it must report the exit code like the
            // other two. `stop()` records it, and so does the live watcher — but
            // this path is the one a *non-detached* box takes when its runtime
            // goes away, which is every `boxlite create` + `boxlite start`. Left
            // as a bare `mark_stop()`, it marked the box Stopped with no code, and
            // `recover_boxes` could not backfill it afterwards (its branch only
            // fires on a box still marked Running). `inspect .State.ExitCode`
            // answered 0 for a command that exited 23.
            record_main_command_exit(
                &mut state,
                &box_layout.container_exit_file(config.container.id.as_str()),
            );
            let _ = self.box_manager.save_box(&config.id, &state);

            let _ = std::fs::remove_file(box_layout.pid_file_path());
        }
    }

    // ========================================================================
    // INTERNAL - BOX OPERATIONS
    // ========================================================================

    /// Resolve an ID or name to the actual box ID.
    ///
    /// Checks in-memory cache first (for boxes not yet persisted), then database.
    fn resolve_id(&self, id_or_name: &str) -> BoxliteResult<BoxID> {
        // Check in-memory cache first
        {
            let sync = self.sync_state.read().unwrap();

            // Try as BoxID first
            if let Some(box_id) = BoxID::parse(id_or_name)
                && let Some(weak) = sync.active_boxes_by_id.get(&box_id)
                && weak.upgrade().is_some()
            {
                return Ok(box_id);
            }

            // Try as name
            if let Some(weak) = sync.active_boxes_by_name.get(id_or_name)
                && let Some(strong) = weak.upgrade()
            {
                return Ok(strong.id().clone());
            }
        }

        // Fall back to DB lookup
        self.box_manager
            .lookup_box_id(id_or_name)?
            .ok_or_else(|| BoxliteError::NotFound(id_or_name.to_string()))
    }

    /// Remove a box from the runtime (internal implementation).
    ///
    /// This is the internal implementation called by both `BoxliteRuntime::remove()`
    /// and `LiteBox::stop()` when its removal policy is enabled.
    ///
    /// Handles both persisted boxes (in database) and in-memory-only boxes
    /// (created but not yet started).
    ///
    /// # Arguments
    /// * `id` - Box ID to remove
    /// * `force` - If true, kill the process first if running
    ///
    /// # Errors
    /// - Box not found
    /// - Box is active and force=false
    pub(crate) fn remove_box(&self, id: &BoxID, force: bool) -> BoxliteResult<()> {
        tracing::debug!(box_id = %id, force = force, "RuntimeInnerImpl::remove_box called");

        // Try to get box from database first
        if let Some((config, state)) = self.box_manager.box_by_id(id)? {
            // Box exists in database - handle as before
            let mut state = state;
            if state.status.is_active() || state.pid.is_some() {
                if force {
                    // Stop the box's whole process tree through the same handler
                    // teardown `LiteBox::stop()` uses: it SIGTERMs the recorded
                    // pid (the outer launcher), then reaps any survivors of a
                    // detached box's inner pid-ns tree (inner bwrap + shim + VM) —
                    // which a single-pid kill misses, since #851 stopped tying
                    // detached boxes to the launcher's lifetime.
                    if let Some(pid) = state.pid {
                        tracing::info!(box_id = %id, pid = pid, "Force stopping box process tree");
                        let mut handler = ShimHandler::from_pid(pid, config.id.clone());
                        let _ = handler.stop();
                    }
                    // Update status to stopped and save
                    state.set_status(BoxStatus::Stopped);
                    state.set_pid(None);
                    self.box_manager.save_box(id, &state)?;
                } else {
                    // Non-force mode: error on active box
                    return Err(BoxliteError::InvalidState(format!(
                        "cannot remove box {} with live pid {:?} (status: {:?}). Use force=true to stop first",
                        id, state.pid, state.status
                    )));
                }
            }

            // Check if other boxes depend on this box's disks (COW backing references).
            if !force {
                let dependents = find_boxes_depending_on(self, id.as_ref())?;
                if !dependents.is_empty() {
                    return Err(BoxliteError::InvalidState(format!(
                        "Cannot remove box: boxes [{}] have clone dependencies on it. \
                         Remove those first or use force=true.",
                        dependents.join(", ")
                    )));
                }
            }

            // Remove from BoxManager (database-first)
            self.box_manager.remove_box(id)?;

            // Free the lock if one was allocated
            if let Some(lock_id) = state.lock_id {
                if let Err(e) = self.lock_manager.free(lock_id) {
                    tracing::warn!(
                        box_id = %id,
                        lock_id = %lock_id,
                        error = %e,
                        "Failed to free lock for removed box"
                    );
                } else {
                    tracing::debug!(
                        box_id = %id,
                        lock_id = %lock_id,
                        "Freed lock for removed box"
                    );
                }
            }

            // 1. Remove all base disk refs for this box (returns affected base IDs for GC)
            let affected_base_ids = self
                .base_disk_mgr
                .store()
                .remove_all_refs_for_box(id.as_ref())
                .unwrap_or_default();

            // 2. Delete container disk file
            let disks_dir = config.box_home.join("disks");
            let container = disks_dir.join(crate::disk::constants::filenames::CONTAINER_DISK);
            let _ = std::fs::remove_file(&container);

            // 3. Remove all snapshots for this box (files + DB records).
            self.snapshot_mgr
                .remove_all_for_box(id.as_ref(), &config.box_home);

            // 4. GC each affected base (may cascade to parents)
            for base_id in affected_base_ids {
                self.base_disk_mgr.try_gc_base(&base_id);
            }

            // Delete box directory + its socket binding symlink
            config.sockets().remove();
            let box_home = config.box_home;
            if box_home.exists()
                && let Err(e) = std::fs::remove_dir_all(&box_home)
            {
                tracing::warn!(
                    box_id = %id,
                    path = %box_home.display(),
                    error = %e,
                    "Failed to cleanup box directory"
                );
            }

            // Invalidate cache
            self.invalidate_box_impl(id, config.name.as_deref());

            tracing::info!(box_id = %id, "Removed box");
            return Ok(());
        }

        // Box not in database - check in-memory cache
        let box_impl = {
            let sync = self.sync_state.read().unwrap();
            sync.active_boxes_by_id
                .get(id)
                .and_then(|weak| weak.upgrade())
        };

        if let Some(box_impl) = box_impl {
            // Box exists in-memory only (not yet started/persisted)
            let state = box_impl.state.read();
            if state.status.is_active() && !force {
                return Err(BoxliteError::InvalidState(format!(
                    "cannot remove active box {} (status: {:?}). Use force=true to stop first",
                    id, state.status
                )));
            }
            drop(state);

            // 1. Remove all base disk refs for this box (returns affected base IDs for GC)
            let affected_base_ids = self
                .base_disk_mgr
                .store()
                .remove_all_refs_for_box(id.as_ref())
                .unwrap_or_default();

            // 2. Delete container disk file
            let disks_dir = box_impl.config.box_home.join("disks");
            let container = disks_dir.join(crate::disk::constants::filenames::CONTAINER_DISK);
            let _ = std::fs::remove_file(&container);

            // 3. Remove all snapshots for this box (files + DB records).
            self.snapshot_mgr
                .remove_all_for_box(id.as_ref(), &box_impl.config.box_home);

            // 4. GC each affected base (may cascade to parents)
            for base_id in affected_base_ids {
                self.base_disk_mgr.try_gc_base(&base_id);
            }

            // Invalidate cache (removes from in-memory maps)
            self.invalidate_box_impl(id, box_impl.config.name.as_deref());

            // Delete box directory + its socket binding symlink
            box_impl.config.sockets().remove();
            let box_home = &box_impl.config.box_home;
            if box_home.exists()
                && let Err(e) = std::fs::remove_dir_all(box_home)
            {
                tracing::warn!(
                    box_id = %id,
                    path = %box_home.display(),
                    error = %e,
                    "Failed to cleanup box directory"
                );
            }

            tracing::info!(box_id = %id, "Removed in-memory box");
            return Ok(());
        }

        // Box not found anywhere
        Err(BoxliteError::NotFound(id.to_string()))
    }

    // ========================================================================
    // INTERNAL - INITIALIZATION
    // ========================================================================

    /// Initialize box variables with defaults.
    ///
    /// Creates config and state for a new box. State starts with Configured status.
    /// Lock allocation and DB persistence happen in create() immediately after this.
    fn init_box_variables(
        &self,
        options: &BoxOptions,
        name: Option<String>,
    ) -> (BoxConfig, BoxState) {
        use crate::litebox::config::ContainerRuntimeConfig;

        // Generate unique box ID (12-char Base62)
        let box_id = BoxIDMint::mint();

        // Generate container ID (64-char hex)
        let container_id = ContainerID::new();

        // Record creation timestamp
        let now = Utc::now();

        // Derive paths from ID (computed from layout + ID)
        let box_home = self.layout.boxes_dir().join(box_id.as_str());
        // Create container runtime config
        let container = ContainerRuntimeConfig { id: container_id };

        // Create config with defaults + user options. Socket paths are NOT
        // stored — they derive from (box_home, id) at point of use via
        // BoxConfig::sockets(), so they can never go stale.
        let config = BoxConfig {
            id: box_id,
            name,
            created_at: now,
            container,
            options: options.clone(),
            engine_kind: VmmKind::Libkrun,
            box_home,
        };

        // Create initial state (status = Configured)
        let state = BoxState::new();

        (config, state)
    }

    /// Provision a new box: create identity, persist config+state, return LiteBox.
    ///
    /// Caller provides `staging_dir` (already created with disks in place) and
    /// `options` for the box configuration. The staging directory is renamed to
    /// the canonical `boxes_dir/<box_id>` path. The box is persisted with the
    /// given `initial_status` (typically `Stopped` for clone/import operations).
    ///
    /// On failure, cleans up the allocated lock and box directory.
    pub(crate) async fn provision_box(
        self: &Arc<Self>,
        staging_dir: std::path::PathBuf,
        name: Option<String>,
        options: BoxOptions,
        initial_status: BoxStatus,
    ) -> BoxliteResult<LiteBox> {
        use crate::litebox::config::ContainerRuntimeConfig;

        let box_id = BoxIDMint::mint();
        let container_id = ContainerID::new();
        let now = Utc::now();

        // Move staging dir to canonical path (cross-fs safe: staging may be on
        // a different filesystem than the boxes dir).
        let box_home = self.layout.boxes_dir().join(box_id.as_str());
        move_dir_cross_fs(&staging_dir, &box_home).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to move {} to {}: {}",
                staging_dir.display(),
                box_home.display(),
                e
            ))
        })?;

        let config = BoxConfig {
            id: box_id.clone(),
            name,
            created_at: now,
            container: ContainerRuntimeConfig { id: container_id },
            options,
            engine_kind: VmmKind::Libkrun,
            box_home,
        };

        let mut state = BoxState::new();
        state.set_status(initial_status);

        let lock_id = self.lock_manager.allocate()?;
        state.set_lock_id(lock_id);

        if let Err(e) = self.box_manager.add_box(&config, &state) {
            let _ = self.lock_manager.free(lock_id);
            let _ = std::fs::remove_dir_all(&config.box_home);
            config.sockets().remove();
            return Err(e);
        }

        self.get(box_id.as_str()).await?.ok_or_else(|| {
            BoxliteError::Internal("Provisioned box not found after persist".to_string())
        })
    }

    /// Recover boxes from persistent storage on runtime startup.
    fn recover_boxes(&self) -> BoxliteResult<()> {
        use crate::util::{PidFileReader, ProcessIdentity};

        // Check for system reboot and reset active boxes
        self.box_manager.check_and_handle_reboot()?;

        // Clear all locks before recovery - safe because we hold the runtime lock.
        // This ensures a clean slate for lock allocation during recovery.
        self.lock_manager.clear_all_locks()?;

        // Phase 0: Scan filesystem for orphaned directories (no DB record)
        // These can occur when:
        // - Box creation succeeded but DB persist failed
        // - Process crashed after directory creation but before DB insert
        // - Old boxes from before persistence was implemented
        self.cleanup_orphaned_directories()?;

        let persisted = self.box_manager.all_boxes(true)?;

        // Phase 1: Clean up boxes that shouldn't persist
        // - remove-on-stop boxes (auto_delete): ephemeral, shouldn't survive restarts
        // - Orphaned active boxes: was Running but directory is missing (crashed mid-operation)
        //
        // Note: We don't remove Configured or Stopped boxes without directories because:
        // - Configured boxes: created but never started, no directory yet (this is valid)
        // - Stopped boxes: might not have a directory if never started
        // - Only Running boxes must have a directory
        let mut boxes_to_remove = Vec::new();
        for (config, state) in &persisted {
            let should_remove = if config.options.removes_on_stop() {
                tracing::info!(
                    box_id = %config.id,
                    "Removing ephemeral (remove-on-stop) box during recovery"
                );
                true
            } else if state.status.is_active() && !config.box_home.exists() {
                // Only remove orphaned boxes that were in an active state
                // Stopped boxes might not have a directory if never started
                tracing::warn!(
                    box_id = %config.id,
                    status = ?state.status,
                    box_home = %config.box_home.display(),
                    "Removing orphaned active box (directory missing) during recovery"
                );
                true
            } else {
                false
            };

            if should_remove {
                boxes_to_remove.push(config.id.clone());
            }
        }

        // Remove invalid boxes from database and cleanup their directories
        for box_id in &boxes_to_remove {
            // Find the config to get box_home path
            if let Some((config, _)) = persisted.iter().find(|(c, _)| &c.id == box_id) {
                // Clean up box directory + binding symlink if they exist
                config.sockets().remove();
                if config.box_home.exists()
                    && let Err(e) = std::fs::remove_dir_all(&config.box_home)
                {
                    tracing::warn!(
                        box_id = %box_id,
                        path = %config.box_home.display(),
                        error = %e,
                        "Failed to cleanup box directory during recovery"
                    );
                }
            }

            // Remove from database
            if let Err(e) = self.box_manager.remove_box(box_id) {
                tracing::warn!(
                    box_id = %box_id,
                    error = %e,
                    "Failed to remove box from database during recovery cleanup"
                );
            }
        }

        if !boxes_to_remove.is_empty() {
            tracing::info!(
                "Cleaned up {} boxes during recovery (remove-on-stop or orphaned)",
                boxes_to_remove.len()
            );
        }

        // Phase 1.5: Recover any pending snapshots that were interrupted by a crash.
        {
            let boxes_dir = self.layout.boxes_dir();
            if boxes_dir.exists()
                && let Ok(entries) = std::fs::read_dir(&boxes_dir)
            {
                for entry in entries.flatten() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                        crate::litebox::local_snapshot::recover_pending_snapshot(&entry.path());
                    }
                }
            }
        }

        // Phase 2: Recover remaining valid boxes
        let persisted = self.box_manager.all_boxes(true)?;

        tracing::info!("Recovering {} boxes from database", persisted.len());

        for (config, mut state) in persisted {
            let box_id = &config.id;
            let original_status = state.status;
            let original_pid = state.pid;

            // Reclaim the lock for this box if one was allocated
            if let Some(lock_id) = state.lock_id {
                match self.lock_manager.allocate_and_retrieve(lock_id) {
                    Ok(_) => {
                        tracing::debug!(
                            box_id = %box_id,
                            lock_id = %lock_id,
                            "Reclaimed lock for recovered box"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            box_id = %box_id,
                            lock_id = %lock_id,
                            error = %e,
                            "Failed to reclaim lock for recovered box"
                        );
                    }
                }
            }

            let box_layout = self.layout.box_layout(box_id.as_str(), false)?;
            let pid_path = box_layout.pid_file_path();
            let exit_path = box_layout.exit_file_path();
            let had_stale_exit = exit_path.exists();

            // Truth-source precedence: a live shim (ProcessIdentity) is
            // ground truth. The exit file is at-best an artifact of a past
            // lifecycle, so it only matters when no shim is alive.
            match PidFileReader::at(&pid_path).process_identity() {
                ProcessIdentity::Verified(pid) => {
                    state.adopt_recovered_shim(pid);
                    // Live shim wins — archive any prior-lifecycle exit
                    // file so the next crash gets the canonical slot.
                    if had_stale_exit {
                        stash_exit_file(&box_layout);
                        tracing::warn!(
                            box_id = %box_id,
                            pid,
                            "Stale exit file alongside live shim; stashed to exit.previous \
                             (post-start cleanup may have been skipped or external interference)"
                        );
                    }
                    tracing::info!(
                        box_id = %box_id,
                        pid,
                        "Recovered running box from PID file"
                    );
                }
                ProcessIdentity::Legacy(pid) => {
                    // Pre-fingerprint PID file. Adopt the live PID so VMs
                    // aren't lost on upgrade; the next stop/start writes a
                    // two-line file and the fingerprint check takes over.
                    state.adopt_recovered_shim(pid);
                    if had_stale_exit {
                        stash_exit_file(&box_layout);
                        tracing::warn!(
                            box_id = %box_id,
                            pid,
                            "Stale exit file alongside live (legacy) shim; stashed to exit.previous"
                        );
                    }
                    tracing::warn!(
                        box_id = %box_id,
                        pid,
                        "Recovered legacy box without start-time fingerprint; \
                         restart to upgrade"
                    );
                }
                ProcessIdentity::Absent => {
                    // PID is dead or missing — clean the stale PID file.
                    let _ = std::fs::remove_file(&pid_path);

                    if had_stale_exit && crate::vmm::ExitInfo::from_file(&exit_path).is_some() {
                        // Shim crashed in a prior lifecycle. Surface as
                        // Failed with the crash report; the exit file
                        // stays as the failure artifact (active slot).
                        let report = crate::litebox::CrashReport::from_exit_file(
                            &exit_path,
                            &box_layout.console_output_path(),
                            &box_layout.stderr_file_path(),
                            box_id.as_str(),
                            None,
                        );
                        state.mark_failed(&report.client_message);
                        tracing::error!(
                            box_id = %box_id,
                            "Box crashed in a prior lifecycle:\n{}",
                            report.operator_report
                        );
                        tracing::warn!(
                            box_id = %box_id,
                            "Box crashed; marked Failed with crash report"
                        );
                    } else if state.status == BoxStatus::Running {
                        // A Running box whose shim is gone: the guest powered
                        // off because its main command exited (docker
                        // semantics), and no live watcher was around to see it
                        // — this process was not running at the time. Take the
                        // exit code from the record the guest left behind.
                        //
                        // The container id lives in the box *config*
                        // (BoxState.container_id is never persisted).
                        record_main_command_exit(
                            &mut state,
                            &box_layout.container_exit_file(config.container.id.as_str()),
                        );
                        tracing::warn!(
                            box_id = %box_id,
                            exit_code = ?state.exit_code,
                            "Shim not verifiable (file missing, process dead, or PID reuse); \
                             marked Stopped"
                        );
                    }
                }
            }

            // Save updated state to database if changed
            if state.status != original_status || state.pid != original_pid {
                self.box_manager.save_box(box_id, &state)?;
            }
        }

        // GC unreferenced guest rootfs entries
        if let Err(e) = self.guest_rootfs_mgr.gc(&self.layout.boxes_dir()) {
            tracing::warn!("Guest rootfs GC failed: {}", e);
        }

        // Then reclaim base files no DB record and no overlay claims — the
        // per-kind collectors above work from records, so a file whose row is
        // gone is invisible to them.
        if let Err(e) = self.base_disk_mgr.gc_orphans(&self.layout.boxes_dir()) {
            tracing::warn!("Orphaned base disk GC failed: {}", e);
        }

        tracing::info!("Box recovery complete");
        Ok(())
    }

    /// Scan filesystem for orphaned box directories and remove them.
    ///
    /// Orphaned directories are those that exist in ~/.boxlite/boxes/
    /// but have no corresponding record in the database. This can occur when:
    /// - Box creation succeeded but database persist failed
    /// - Process crashed after directory creation but before DB insert
    /// - Old boxes from before persistence was implemented
    fn cleanup_orphaned_directories(&self) -> BoxliteResult<()> {
        use std::collections::HashSet;

        let boxes_dir = self.layout.boxes_dir();
        if !boxes_dir.exists() {
            return Ok(());
        }

        // Scan filesystem for box directories
        let fs_box_ids: HashSet<String> = match std::fs::read_dir(&boxes_dir) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                .filter_map(|entry| entry.file_name().to_str().map(String::from))
                .collect(),
            Err(e) => {
                tracing::warn!(
                    path = %boxes_dir.display(),
                    error = %e,
                    "Failed to scan boxes directory for orphans"
                );
                return Ok(()); // Non-fatal, continue with recovery
            }
        };

        if fs_box_ids.is_empty() {
            return Ok(());
        }

        // Load all box IDs from database
        let db_box_ids: HashSet<String> = self
            .box_manager
            .all_boxes(false)?
            .into_iter()
            .map(|(cfg, _)| cfg.id.to_string())
            .collect();

        // Find orphaned directories (exist on filesystem but not in DB)
        let orphaned: Vec<_> = fs_box_ids.difference(&db_box_ids).collect();

        if orphaned.is_empty() {
            return Ok(());
        }

        tracing::info!(
            count = orphaned.len(),
            "Found orphaned box directories (no DB record)"
        );

        for orphan_id in orphaned {
            let orphan_dir = boxes_dir.join(orphan_id);
            tracing::warn!(
                box_id = %orphan_id,
                path = %orphan_dir.display(),
                "Removing orphaned box directory (no database record)"
            );

            crate::net::socket_path::BoxSockets::new(
                orphan_id.clone(),
                orphan_dir.join(crate::runtime::layout::dirs::SOCKETS_DIR),
            )
            .remove();
            if let Err(e) = std::fs::remove_dir_all(&orphan_dir) {
                tracing::error!(
                    box_id = %orphan_id,
                    path = %orphan_dir.display(),
                    error = %e,
                    "Failed to remove orphaned box directory"
                );
            }
        }

        Ok(())
    }

    // ========================================================================
    // INTERNAL - BOX IMPL CACHE
    // ========================================================================

    /// Get existing BoxImpl from cache or create new one.
    ///
    /// Returns `(SharedBoxImpl, inserted)` where `inserted` is true if a new BoxImpl
    /// was created, false if an existing one was returned.
    ///
    /// Checks both by name (if provided) and by ID. This prevents duplicate names
    /// even for boxes not yet persisted to database.
    fn get_or_create_box_impl(
        self: &Arc<Self>,
        config: BoxConfig,
        state: BoxState,
    ) -> (SharedBoxImpl, bool) {
        use crate::litebox::box_impl::BoxImpl;

        let box_id = config.id.clone();
        let box_name = config.name.clone();

        let mut sync = self.sync_state.write().unwrap();

        // Check by name first (if provided) - prevents duplicate names
        if let Some(ref name) = box_name
            && let Some(weak) = sync.active_boxes_by_name.get(name)
        {
            if let Some(strong) = weak.upgrade() {
                tracing::trace!(name = %name, "Reusing cached BoxImpl by name");
                return (strong, false);
            }
            // Dead weak ref, clean it up
            sync.active_boxes_by_name.remove(name);
        }

        // Check by ID
        if let Some(weak) = sync.active_boxes_by_id.get(&box_id) {
            if let Some(strong) = weak.upgrade() {
                tracing::trace!(box_id = %box_id, "Reusing cached BoxImpl by ID");
                return (strong, false);
            }
            // Dead weak ref, clean it up
            sync.active_boxes_by_id.remove(&box_id);
        }

        // Create new BoxImpl and cache in both maps
        // Pass a child token so box can be cancelled independently or via runtime shutdown
        let box_token = self.shutdown_token.child_token();
        let box_impl = Arc::new(BoxImpl::new(config, state, Arc::clone(self), box_token));
        let weak = Arc::downgrade(&box_impl);

        sync.active_boxes_by_id.insert(box_id.clone(), weak.clone());
        if let Some(name) = box_name {
            sync.active_boxes_by_name.insert(name.clone(), weak);
            tracing::trace!(box_id = %box_id, name = %name, "Created and cached new BoxImpl");
        } else {
            tracing::trace!(box_id = %box_id, "Created and cached new BoxImpl (unnamed)");
        }

        (box_impl, true)
    }

    /// Remove BoxImpl from cache.
    ///
    /// Called when box is stopped or removed. Existing handles become stale;
    /// new handles from runtime.get() will get a fresh BoxImpl.
    pub(crate) fn invalidate_box_impl(&self, box_id: &BoxID, box_name: Option<&str>) {
        let mut sync = self.sync_state.write().unwrap();
        sync.active_boxes_by_id.remove(box_id);
        if let Some(name) = box_name {
            sync.active_boxes_by_name.remove(name);
        }
        tracing::trace!(box_id = %box_id, name = ?box_name, "Invalidated BoxImpl cache");
    }

    /// Acquire coordination lock for multi-step atomic operations.
    ///
    /// Use this when you need atomicity across multiple operations on
    /// box_manager or image_manager.
    #[allow(unused)]
    pub(crate) fn acquire_write(
        &self,
    ) -> BoxliteResult<std::sync::RwLockWriteGuard<'_, SynchronizedState>> {
        self.sync_state
            .write()
            .map_err(|e| BoxliteError::Internal(format!("Coordination lock poisoned: {}", e)))
    }
}

/// Find boxes that depend on bases created from this box.
///
/// Uses the `base_disk_ref` table: looks up bases where `source_box_id` matches,
/// then finds other boxes that reference those bases.
///
/// Used to prevent removing a box that other boxes depend on (COW clones).
fn find_boxes_depending_on(runtime: &RuntimeImpl, box_id: &str) -> BoxliteResult<Vec<String>> {
    use std::collections::HashSet;

    let bases = runtime.base_disk_mgr.store().list_by_box(box_id, None)?;

    let mut dependents = HashSet::new();
    for base in &bases {
        for dep_box_id in runtime.base_disk_mgr.store().dependent_boxes(base.id())? {
            if dep_box_id != box_id {
                dependents.insert(dep_box_id);
            }
        }
    }
    Ok(dependents.into_iter().collect())
}

/// Reject qcow2 disks that contain backing file references.
///
/// Imported disks must be standalone — a backing reference could point to
/// arbitrary host files (e.g. /etc/shadow) and leak their contents.
impl std::fmt::Debug for RuntimeImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeInner")
            .field("home_dir", &self.layout.home_dir())
            .finish()
    }
}

// ============================================================================
// LocalRuntime — RuntimeBackend adapter for local VM execution
// ============================================================================

/// Adapter bridging `RuntimeImpl` (Arc-receiver methods) to `RuntimeBackend` trait.
///
/// `RuntimeImpl` methods use `self: &Arc<Self>` for back-references from `BoxImpl`.
/// Trait methods use `&self`. This newtype holds the Arc as a field to bridge the gap.
pub(crate) struct LocalRuntime(pub(crate) SharedRuntimeImpl);

fn reject_local_unsupported_options(options: &BoxOptions) -> BoxliteResult<()> {
    // Local runtimes support auto_delete as a remove-on-stop policy, but AutoStop
    // needs a sweeper that the local runtime does not have, so it stays REST-only.
    if options.auto_stop.is_some_and(|seconds| seconds > 0) {
        return Err(BoxliteError::Unsupported(
            "AutoStop is only supported by REST runtimes".into(),
        ));
    }

    Ok(())
}

/// Replace every managed-volume reference with the directory backing it.
///
/// The reference reaches the runtime as `managed_volume` — an id or a name —
/// and only the store can say where that volume's payload lives. A name the
/// store has never seen is created here, so `-v my-data:/data` works on first
/// use the way it does with docker. Runs in `create_inner` only once a new box
/// is certain, so reusing an existing box or clashing on a name creates
/// nothing; resolving at create rather than at boot keeps any failure where
/// the caller still holds the error, instead of part-way through
/// `resolve_user_volumes`.
fn resolve_managed_volumes(
    store: &crate::volumes::LocalNamedVolumeStore,
    options: &mut BoxOptions,
) -> BoxliteResult<()> {
    for volume in &mut options.volumes {
        let Some(reference) = volume.managed_volume.take() else {
            continue;
        };
        volume.host_path = store
            .payload_dir(&reference)?
            .to_string_lossy()
            .into_owned();
    }
    Ok(())
}

async fn sanitize_local_options(
    features: &ExperimentalFeatures,
    options: BoxOptions,
) -> BoxliteResult<BoxOptions> {
    reject_local_unsupported_options(&options)?;
    features.require_for_options(&options)?;
    tokio::task::spawn_blocking(move || {
        let mut options = options;
        options.sanitize()?;
        Ok(options)
    })
    .await
    .map_err(|error| {
        BoxliteError::Internal(format!(
            "failed to join box option validation task: {error}"
        ))
    })?
}

#[async_trait::async_trait]
impl super::backend::RuntimeBackend for LocalRuntime {
    async fn create(&self, options: BoxOptions, name: Option<String>) -> BoxliteResult<LiteBox> {
        let options = sanitize_local_options(&self.0.experimental_features, options).await?;
        self.0.create(options, name).await
    }

    async fn get_or_create(
        &self,
        options: BoxOptions,
        name: Option<String>,
    ) -> BoxliteResult<(LiteBox, bool)> {
        let options = sanitize_local_options(&self.0.experimental_features, options).await?;
        self.0.get_or_create(options, name).await
    }

    async fn get(&self, id_or_name: &str) -> BoxliteResult<Option<LiteBox>> {
        self.0.get(id_or_name).await
    }

    async fn get_info(&self, id_or_name: &str) -> BoxliteResult<Option<BoxInfo>> {
        self.0.get_info(id_or_name).await
    }

    async fn list_info(&self) -> BoxliteResult<Vec<BoxInfo>> {
        self.0.list_info().await
    }

    async fn exists(&self, id_or_name: &str) -> BoxliteResult<bool> {
        self.0.exists(id_or_name).await
    }

    async fn metrics(&self) -> BoxliteResult<RuntimeMetrics> {
        Ok(self.0.metrics().await)
    }

    async fn remove(&self, id_or_name: &str, force: bool) -> BoxliteResult<()> {
        self.0.remove(id_or_name, force)
    }

    async fn shutdown(&self, timeout: Option<i32>) -> BoxliteResult<()> {
        self.0.shutdown(timeout).await
    }

    async fn import_box(
        &self,
        archive: BoxArchive,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        self.0.import_box(archive, name).await
    }

    fn shutdown_sync(&self) {
        self.0.shutdown_sync();
    }
}

// Image operations (separate from RuntimeBackend)
#[async_trait::async_trait]
impl super::images::ImageBackend for LocalRuntime {
    async fn pull_image(&self, image_ref: &str) -> BoxliteResult<crate::images::ImageObject> {
        if self.0.shutdown_token.is_cancelled() {
            return Err(BoxliteError::Stopped(
                "Cannot pull image: runtime has been shut down".into(),
            ));
        }
        self.0.image_manager.pull(image_ref).await
    }

    async fn list_images(&self) -> BoxliteResult<Vec<crate::runtime::types::ImageInfo>> {
        if self.0.shutdown_token.is_cancelled() {
            return Err(BoxliteError::Stopped(
                "Cannot list images: runtime has been shut down".into(),
            ));
        }
        self.0.image_manager.list().await
    }
}

/// Run one [`LocalNamedVolumeStore`](crate::volumes::LocalNamedVolumeStore) call off the
/// async runtime.
///
/// Every store method is synchronous filesystem IO — `remove` on a
/// multi-gigabyte volume would pin a tokio worker for the whole
/// `remove_dir_all` — so it belongs on the blocking pool, like the DB calls
/// elsewhere in this file. `what` names the operation in the shutdown error.
async fn run_volume_op<T, F>(runtime: &SharedRuntimeImpl, what: &str, op: F) -> BoxliteResult<T>
where
    F: FnOnce(crate::volumes::LocalNamedVolumeStore) -> BoxliteResult<T> + Send + 'static,
    T: Send + 'static,
{
    if runtime.shutdown_token.is_cancelled() {
        return Err(BoxliteError::Stopped(format!(
            "Cannot {what}: runtime has been shut down"
        )));
    }
    let store = crate::volumes::LocalNamedVolumeStore::new(runtime.layout.home_dir());
    tokio::task::spawn_blocking(move || op(store))
        .await
        .map_err(|error| {
            BoxliteError::Internal(format!("failed to join volume store task: {error}"))
        })?
}

// Named-volume operations (separate from RuntimeBackend). Each volume is a
// directory under `{home}/volumes/`, managed by `LocalNamedVolumeStore`.
#[async_trait::async_trait]
impl super::volumes::VolumeBackend for LocalRuntime {
    async fn create_volume(&self, name: Option<&str>) -> BoxliteResult<crate::volumes::VolumeInfo> {
        let name = name.map(str::to_string);
        run_volume_op(&self.0, "create volume", move |store| {
            store.create(name.as_deref())
        })
        .await
    }

    async fn list_volumes(&self) -> BoxliteResult<Vec<crate::volumes::VolumeInfo>> {
        run_volume_op(&self.0, "list volumes", |store| store.list()).await
    }

    async fn get_volume(&self, id: &str) -> BoxliteResult<crate::volumes::VolumeInfo> {
        let id = id.to_string();
        run_volume_op(&self.0, "get volume", move |store| store.get(&id)).await
    }

    async fn remove_volume(&self, id: &str, force: bool) -> BoxliteResult<()> {
        let id = id.to_string();
        run_volume_op(&self.0, "remove volume", move |store| {
            store.remove(&id, force)
        })
        .await
    }
}

// ============================================================================
// Drop — Safety net for non-default runtimes
// ============================================================================

impl Drop for RuntimeImpl {
    fn drop(&mut self) {
        if self.shutdown_token.is_cancelled() {
            return; // shutdown() was already called
        }
        // Safety net: stop non-detached boxes if shutdown() wasn't called.
        // shutdown_sync() logs per-box when it actually stops something.
        self.shutdown_sync();
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experimental::custom_kernel::KernelOptions;
    use crate::litebox::config::{BoxConfig, ContainerRuntimeConfig};
    use crate::runtime::backend::RuntimeBackend;
    use crate::runtime::images::ImageBackend;
    use crate::runtime::options::RootfsSpec;
    use crate::vmm::guest_binary::GuestBinary;
    use tempfile::TempDir;

    /// `IMAGE_DISK_GUEST_BINARY_HEADROOM_BYTES` must stay ahead of the real
    /// embedded `boxlite-guest` binary — silently falling behind would
    /// reintroduce the exact "Could not allocate block in ext2 filesystem"
    /// failure this constant exists to prevent, undetected until a real box
    /// tried to boot. Skipped when the guest binary isn't assembled (no
    /// `make guest`/`make runtime:debug`).
    #[test]
    fn guest_binary_fits_in_image_disk_headroom() {
        let Ok(guest) = GuestBinary::get() else {
            eprintln!("skipping: boxlite-guest not found (run `make guest`)");
            return;
        };
        let guest_len = std::fs::metadata(guest.path())
            .expect("stat resolved guest binary")
            .len();
        assert!(
            guest_len < IMAGE_DISK_GUEST_BINARY_HEADROOM_BYTES,
            "guest binary is {} MiB, headroom budget is only {} MiB -- bump \
             IMAGE_DISK_GUEST_BINARY_HEADROOM_BYTES",
            guest_len / (1024 * 1024),
            IMAGE_DISK_GUEST_BINARY_HEADROOM_BYTES / (1024 * 1024)
        );
    }

    #[tokio::test]
    async fn local_runtime_rejects_explicit_lifecycle_policy() {
        let features = ExperimentalFeatures::default();
        let mut options = BoxOptions::default();
        assert!(
            sanitize_local_options(&features, options.clone())
                .await
                .is_ok()
        );

        options.auto_stop = Some(0);
        assert!(
            sanitize_local_options(&features, options.clone())
                .await
                .is_ok()
        );

        options.auto_stop = Some(1);
        assert!(matches!(
            sanitize_local_options(&features, options.clone()).await,
            Err(BoxliteError::Unsupported(_))
        ));
        options.auto_stop = None;
        options.auto_delete = Some(3600);
        assert!(sanitize_local_options(&features, options).await.is_ok());
    }

    /// A managed volume names a volume, not a path; the runtime asks its store
    /// where that volume lives while creating the box and persists the answer,
    /// so boot never has to know the store exists.
    #[tokio::test]
    async fn create_resolves_managed_volumes_into_the_persisted_config() {
        use crate::runtime::options::VolumeSpec;

        let (runtime, _dir) = create_test_runtime();
        let store = crate::volumes::LocalNamedVolumeStore::new(runtime.layout.home_dir());
        let volume = store.create(Some("my-data")).expect("create volume");
        let local = LocalRuntime(runtime.clone());

        local
            .create(
                BoxOptions {
                    volumes: vec![VolumeSpec::managed_volume("my-data", "/data")],
                    ..Default::default()
                },
                Some("known".to_string()),
            )
            .await
            .expect("a known volume resolves");
        let (config, _) = runtime.box_manager.lookup_box("known").unwrap().unwrap();
        assert_eq!(
            runtime
                .layout
                .home_dir()
                .join("volumes")
                .join(&volume.id)
                .join("_data")
                .to_string_lossy(),
            config.options.volumes[0].host_path,
            "the persisted mount points at the volume's payload directory"
        );
        assert!(
            config.options.volumes[0].managed_volume.is_none(),
            "the reference is consumed once resolved"
        );

        local
            .create(
                BoxOptions {
                    volumes: vec![VolumeSpec::managed_volume("fresh-vol", "/data")],
                    ..Default::default()
                },
                Some("fresh".to_string()),
            )
            .await
            .expect("an unknown reference becomes a new volume, as with docker");
        let fresh = store
            .get("fresh-vol")
            .expect("the volume exists afterwards");
        let (config, _) = runtime.box_manager.lookup_box("fresh").unwrap().unwrap();
        assert!(
            config.options.volumes[0]
                .host_path
                .ends_with(&format!("{}/_data", fresh.id)),
            "the new volume backs the mount: {}",
            config.options.volumes[0].host_path
        );
    }

    /// Reusing an existing box ignores the request's options, so it must not
    /// create the volumes those options name.
    #[tokio::test]
    async fn get_or_create_of_an_existing_box_creates_no_volume() {
        use crate::runtime::options::VolumeSpec;

        let (runtime, _dir) = create_test_runtime();
        let mut config = test_box_config_in_layout(false, &runtime);
        config.name = Some("existing".to_string());
        runtime
            .box_manager
            .add_box(&config, &BoxState::new())
            .unwrap();

        let request = BoxOptions {
            volumes: vec![VolumeSpec::managed_volume("new-volume", "/data")],
            ..Default::default()
        };
        let (_, created) = LocalRuntime(runtime.clone())
            .get_or_create(request, Some("existing".to_string()))
            .await
            .unwrap();
        assert!(!created);

        let store = crate::volumes::LocalNamedVolumeStore::new(runtime.layout.home_dir());
        assert!(matches!(
            store.get("new-volume"),
            Err(BoxliteError::NotFound(_))
        ));
        assert!(
            store.list().unwrap().is_empty(),
            "reuse must not create volumes"
        );
    }

    /// A name clash is reported before any volume is touched — docker reserves
    /// the name before it registers mounts (daemon/create.go:208 vs :249).
    #[tokio::test]
    async fn create_with_a_taken_name_creates_no_volume() {
        use crate::runtime::options::VolumeSpec;

        let (runtime, _dir) = create_test_runtime();
        let mut config = test_box_config_in_layout(false, &runtime);
        config.name = Some("taken".to_string());
        runtime
            .box_manager
            .add_box(&config, &BoxState::new())
            .unwrap();

        let result = LocalRuntime(runtime.clone())
            .create(
                BoxOptions {
                    volumes: vec![VolumeSpec::managed_volume("new-volume", "/data")],
                    ..Default::default()
                },
                Some("taken".to_string()),
            )
            .await;
        match result {
            Err(BoxliteError::InvalidArgument(_)) => {}
            Err(other) => panic!("expected a name clash, got {other:?}"),
            Ok(_) => panic!("a taken name must be rejected"),
        }
        let store = crate::volumes::LocalNamedVolumeStore::new(runtime.layout.home_dir());
        assert!(
            store.list().unwrap().is_empty(),
            "a name clash must not create volumes"
        );
    }

    #[test]
    fn options_compatibility_normalizes_capability_names() {
        let mut actual = test_box_config(false);
        actual
            .options
            .advanced
            .set_capabilities(Some(crate::ContainerCapabilities {
                add: vec!["NET_ADMIN".to_string(), "CAP_SYS_ADMIN".to_string()],
                ..Default::default()
            }))
            .unwrap();
        let mut requested_advanced = crate::AdvancedBoxOptions::default();
        requested_advanced
            .set_capabilities(Some(crate::ContainerCapabilities {
                add: vec!["sys_admin".to_string(), "CAP_NET_ADMIN".to_string()],
                ..Default::default()
            }))
            .unwrap();
        let requested = BoxOptions {
            advanced: requested_advanced,
            ..Default::default()
        };

        assert!(RuntimeImpl::check_options_compatibility(&requested, &actual).is_ok());
    }

    #[test]
    fn check_options_compatibility_rejects_non_privileged_request_reusing_a_privileged_box() {
        // A privileged box's effective capabilities are `add=["ALL"]` — the
        // same shape a non-privileged request can reach explicitly. Matching
        // only on effective capabilities would let this request silently
        // adopt the privileged box and inherit its cleared readonly paths
        // and writable /sys, even though it never asked for privileged mode.
        let mut actual = test_box_config(false);
        actual.options.advanced.privileged = true;

        let mut requested_advanced = crate::AdvancedBoxOptions::default();
        requested_advanced
            .set_capabilities(Some(crate::ContainerCapabilities {
                add: vec!["ALL".to_string()],
                ..Default::default()
            }))
            .unwrap();
        let requested = BoxOptions {
            advanced: requested_advanced,
            ..Default::default()
        };

        assert!(matches!(
            RuntimeImpl::check_options_compatibility(&requested, &actual),
            Err(BoxliteError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn get_or_create_rejects_incompatible_existing_options() {
        let (runtime, _dir) = create_test_runtime();
        let mut config = test_box_config_in_layout(false, &runtime);
        config.name = Some("existing".to_string());
        config
            .options
            .advanced
            .set_capabilities(Some(crate::ContainerCapabilities {
                drop: vec!["NET_RAW".to_string()],
                ..Default::default()
            }))
            .unwrap();
        runtime
            .box_manager
            .add_box(&config, &BoxState::new())
            .unwrap();

        let result = runtime
            .get_or_create(BoxOptions::default(), Some("existing".to_string()))
            .await;

        assert!(matches!(result, Err(BoxliteError::InvalidArgument(_))));
    }

    #[tokio::test]
    async fn local_option_validation_uses_injected_features() {
        let mut options = BoxOptions::default();
        options.advanced.kernel = Some(KernelOptions::new("/definitely/missing/vmlinux"));

        let disabled_error =
            sanitize_local_options(&ExperimentalFeatures::default(), options.clone())
                .await
                .expect_err("custom kernel must be disabled by default");
        assert!(
            disabled_error
                .to_string()
                .contains("BOXLITE_EXPERIMENTAL=custom-kernel")
        );

        let enabled = ExperimentalFeatures::parse("custom-kernel").unwrap();
        let validation_error = sanitize_local_options(&enabled, options)
            .await
            .expect_err("enabled feature should continue to kernel validation");
        assert!(
            validation_error
                .to_string()
                .contains("custom kernel must be a regular file")
        );
    }

    #[tokio::test]
    async fn nested_virtualization_validation_uses_injected_features() {
        let mut advanced = crate::runtime::advanced_options::AdvancedBoxOptions::default();
        advanced.nested_virtualization = true;
        let options = BoxOptions {
            advanced,
            ..Default::default()
        };

        let error = sanitize_local_options(&ExperimentalFeatures::default(), options.clone())
            .await
            .expect_err("nested virtualization must be disabled by default");
        assert!(
            error
                .to_string()
                .contains("BOXLITE_EXPERIMENTAL=nested-virtualization")
        );

        let enabled = ExperimentalFeatures::parse("nested-virtualization").unwrap();
        sanitize_local_options(&enabled, options).await.unwrap();
    }

    #[tokio::test]
    async fn privileged_options_do_not_require_experimental_features() {
        let mut advanced = crate::runtime::advanced_options::AdvancedBoxOptions::default();
        advanced.privileged = true;
        let options = BoxOptions {
            advanced,
            ..Default::default()
        };

        sanitize_local_options(&ExperimentalFeatures::default(), options)
            .await
            .expect("privileged mode should be available without an experimental opt-in");
    }

    /// Create a RuntimeImpl with isolated temp directory.
    fn create_test_runtime() -> (SharedRuntimeImpl, TempDir) {
        let temp_dir = TempDir::new_in("/tmp").expect("Failed to create temp dir");
        let options = BoxliteOptions {
            home_dir: temp_dir.path().to_path_buf(),
            image_registries: vec![],
        };
        let runtime = RuntimeImpl::new(options).expect("Failed to create runtime");
        (runtime, temp_dir)
    }

    fn create_test_runtime_without_host_preflight() -> (SharedRuntimeImpl, TempDir) {
        let temp_dir = TempDir::new_in("/tmp").expect("Failed to create temp dir");
        let options = BoxliteOptions {
            home_dir: temp_dir.path().to_path_buf(),
            image_registries: vec![],
        };
        let runtime = RuntimeImpl::initialize(options, ExperimentalFeatures::default())
            .expect("Failed to create test runtime");
        (runtime, temp_dir)
    }

    /// Create a minimal BoxConfig for testing.
    fn test_box_config(detach: bool) -> BoxConfig {
        BoxConfig {
            id: BoxIDMint::mint(),
            name: None,
            created_at: Utc::now(),
            container: ContainerRuntimeConfig {
                id: ContainerID::new(),
            },
            options: BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                detach,
                auto_delete: Some(0),
                ..Default::default()
            },
            engine_kind: VmmKind::Libkrun,
            box_home: std::path::PathBuf::from("/tmp/test-box"),
        }
    }

    /// Create a BoxState with Running status and a given PID.
    fn running_state(pid: u32) -> BoxState {
        let mut state = BoxState::new();
        state.status = BoxStatus::Running;
        state.pid = Some(pid);
        state
    }

    /// Spawn a dummy sleep process and return its PID.
    fn spawn_dummy_process() -> (u32, std::process::Child) {
        let child = std::process::Command::new("sleep")
            .arg("300")
            .spawn()
            .expect("Failed to spawn dummy process");
        let pid = child.id();
        (pid, child)
    }

    struct ChildGuard(std::process::Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spawn a process that ignores SIGTERM (for force-kill testing).
    fn spawn_sigterm_ignoring_process() -> (u32, std::process::Child) {
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 300")
            .spawn()
            .expect("Failed to spawn SIGTERM-ignoring process");
        let pid = child.id();
        // Wait for the trap to be installed
        std::thread::sleep(std::time::Duration::from_millis(200));
        (pid, child)
    }

    /// Create a BoxConfig whose box_home aligns with the runtime's layout.
    /// This is needed for tests that verify PID file operations.
    fn test_box_config_in_layout(detach: bool, runtime: &RuntimeImpl) -> BoxConfig {
        let id = BoxIDMint::mint();
        let box_home = runtime.layout.boxes_dir().join(id.as_str());
        BoxConfig {
            id,
            name: None,
            created_at: Utc::now(),
            container: ContainerRuntimeConfig {
                id: ContainerID::new(),
            },
            options: BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                detach,
                auto_delete: Some(0),
                ..Default::default()
            },
            engine_kind: VmmKind::Libkrun,
            box_home,
        }
    }

    #[tokio::test]
    async fn get_info_does_not_create_cached_box_handle() {
        let (runtime, _dir) = create_test_runtime();
        let config = test_box_config(false);
        let state = BoxState::new();
        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let is_cached = || {
            runtime
                .sync_state
                .read()
                .unwrap()
                .active_boxes_by_id
                .contains_key(&config.id)
        };
        assert!(!is_cached());

        let info = runtime
            .get_info(config.id.as_str())
            .await
            .expect("Failed to query box info")
            .expect("Box info must exist");

        assert_eq!(info.id, config.id);
        assert!(!is_cached(), "metadata lookup must not cache a live handle");
    }

    #[tokio::test]
    async fn list_info_prefers_fresh_database_state_over_cached_box() {
        let (runtime, _dir) = create_test_runtime();
        let config = test_box_config(false);
        let cached_state = BoxState::new();
        runtime
            .box_manager
            .add_box(&config, &cached_state)
            .expect("Failed to add box");

        let (cached_box, inserted) = runtime.get_or_create_box_impl(config.clone(), cached_state);
        assert!(inserted);
        assert_eq!(cached_box.info().status, BoxStatus::Configured);

        let mut fresh_state = BoxState::new();
        fresh_state.status = BoxStatus::Stopped;
        runtime
            .box_manager
            .save_box(&config.id, &fresh_state)
            .expect("Failed to update box state");

        let infos = runtime.list_info().await.expect("Failed to list box info");

        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].status, BoxStatus::Stopped);
        assert_eq!(cached_box.info().status, BoxStatus::Configured);
    }

    // ====================================================================
    // shutdown() tests
    // ====================================================================

    #[tokio::test]
    async fn test_shutdown_is_idempotent() {
        let (runtime, _dir) = create_test_runtime();

        // First shutdown should succeed
        let result1 = runtime.shutdown(None).await;
        assert!(result1.is_ok());
        assert!(runtime.shutdown_token.is_cancelled());

        // Second shutdown should also succeed (no-op)
        let result2 = runtime.shutdown(None).await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown_cancels_token() {
        let (runtime, _dir) = create_test_runtime();

        assert!(!runtime.shutdown_token.is_cancelled());
        runtime.shutdown(None).await.unwrap();
        assert!(runtime.shutdown_token.is_cancelled());
    }

    #[tokio::test]
    async fn test_shutdown_with_empty_active_boxes() {
        let (runtime, _dir) = create_test_runtime();

        // No boxes created — shutdown should complete cleanly
        let result = runtime.shutdown(Some(1)).await;
        assert!(result.is_ok());
    }

    // ====================================================================
    // shutdown_sync() tests
    // ====================================================================

    #[test]
    fn test_shutdown_sync_cancels_token() {
        let (runtime, _dir) = create_test_runtime();

        assert!(!runtime.shutdown_token.is_cancelled());
        runtime.shutdown_sync();
        assert!(runtime.shutdown_token.is_cancelled());
    }

    #[test]
    fn test_shutdown_sync_is_idempotent() {
        let (runtime, _dir) = create_test_runtime();

        runtime.shutdown_sync();
        assert!(runtime.shutdown_token.is_cancelled());

        // Second call should be no-op (token already cancelled)
        runtime.shutdown_sync();
        assert!(runtime.shutdown_token.is_cancelled());
    }

    #[test]
    fn test_shutdown_sync_stops_non_detached_running_box() {
        let (runtime, _dir) = create_test_runtime();

        // Spawn a dummy process to simulate a shim
        let (pid, mut child) = spawn_dummy_process();

        // Insert a non-detached Running box into the DB
        let config = test_box_config(false); // detach=false
        let state = running_state(pid);
        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        // shutdown_sync should kill the process
        runtime.shutdown_sync();

        // Process should be dead
        let wait_result = child.try_wait().expect("Failed to check child");
        // Give a moment for the process to die
        if wait_result.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert!(
            !crate::util::is_process_alive(pid),
            "Non-detached process should be killed by shutdown_sync"
        );

        // Verify DB state updated to Stopped
        let (_, updated_state) = runtime
            .box_manager
            .box_by_id(&config.id)
            .expect("Failed to query box")
            .expect("Box should exist");
        assert_eq!(updated_state.status, BoxStatus::Stopped);
        assert!(updated_state.pid.is_none());
    }

    #[test]
    fn test_shutdown_sync_skips_detached_box() {
        let (runtime, _dir) = create_test_runtime();

        // Spawn a dummy process to simulate a detached shim
        let (pid, mut child) = spawn_dummy_process();

        // Insert a detached Running box into the DB
        let config = test_box_config(true); // detach=true
        let state = running_state(pid);
        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        // shutdown_sync should skip detached boxes
        runtime.shutdown_sync();

        // Process should still be alive
        assert!(
            crate::util::is_process_alive(pid),
            "Detached process should NOT be killed by shutdown_sync"
        );

        // Verify DB state NOT changed (still Running)
        let (_, db_state) = runtime
            .box_manager
            .box_by_id(&config.id)
            .expect("Failed to query box")
            .expect("Box should exist");
        assert_eq!(db_state.status, BoxStatus::Running);
        assert_eq!(db_state.pid, Some(pid));

        // Cleanup
        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn test_shutdown_sync_skips_stopped_box() {
        let (runtime, _dir) = create_test_runtime();

        // Insert a Stopped box into the DB
        let config = test_box_config(false);
        let mut state = BoxState::new();
        state.status = BoxStatus::Stopped;
        state.pid = None;
        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        // shutdown_sync should skip non-running boxes
        runtime.shutdown_sync();

        // Verify DB state unchanged
        let (_, db_state) = runtime
            .box_manager
            .box_by_id(&config.id)
            .expect("Failed to query box")
            .expect("Box should exist");
        assert_eq!(db_state.status, BoxStatus::Stopped);
    }

    #[test]
    fn test_shutdown_sync_skips_dead_pid() {
        let (runtime, _dir) = create_test_runtime();

        // Use an invalid PID (not a real process)
        let dead_pid = 999_999_999u32;

        let config = test_box_config(false);
        let state = running_state(dead_pid);
        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        // shutdown_sync should skip dead PIDs without errors
        runtime.shutdown_sync();

        // DB state should NOT be updated (process was already dead)
        let (_, db_state) = runtime
            .box_manager
            .box_by_id(&config.id)
            .expect("Failed to query box")
            .expect("Box should exist");
        assert_eq!(db_state.status, BoxStatus::Running);
    }

    #[test]
    fn test_shutdown_sync_mixed_boxes() {
        let (runtime, _dir) = create_test_runtime();

        // Spawn two dummy processes
        let (pid_regular, mut child_regular) = spawn_dummy_process();
        let (pid_detached, mut child_detached) = spawn_dummy_process();

        // Non-detached running box
        let config_regular = test_box_config(false);
        let state_regular = running_state(pid_regular);
        runtime
            .box_manager
            .add_box(&config_regular, &state_regular)
            .unwrap();

        // Detached running box
        let config_detached = test_box_config(true);
        let state_detached = running_state(pid_detached);
        runtime
            .box_manager
            .add_box(&config_detached, &state_detached)
            .unwrap();

        // Stopped box (should be skipped regardless)
        let config_stopped = test_box_config(false);
        let mut state_stopped = BoxState::new();
        state_stopped.status = BoxStatus::Stopped;
        runtime
            .box_manager
            .add_box(&config_stopped, &state_stopped)
            .unwrap();

        runtime.shutdown_sync();

        // Non-detached box: killed, DB updated
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !crate::util::is_process_alive(pid_regular),
            "Non-detached box should be killed"
        );
        let (_, db_regular) = runtime
            .box_manager
            .box_by_id(&config_regular.id)
            .unwrap()
            .unwrap();
        assert_eq!(db_regular.status, BoxStatus::Stopped);

        // Detached box: still alive, DB unchanged
        assert!(
            crate::util::is_process_alive(pid_detached),
            "Detached box should survive"
        );
        let (_, db_detached) = runtime
            .box_manager
            .box_by_id(&config_detached.id)
            .unwrap()
            .unwrap();
        assert_eq!(db_detached.status, BoxStatus::Running);

        // Stopped box: DB unchanged
        let (_, db_stopped) = runtime
            .box_manager
            .box_by_id(&config_stopped.id)
            .unwrap()
            .unwrap();
        assert_eq!(db_stopped.status, BoxStatus::Stopped);

        // Cleanup
        child_regular.kill().ok();
        child_regular.wait().ok();
        child_detached.kill().ok();
        child_detached.wait().ok();
    }

    // ====================================================================
    // Drop tests
    // ====================================================================

    #[test]
    fn test_drop_triggers_shutdown_sync_when_not_cancelled() {
        let (runtime, _dir) = create_test_runtime();

        // Spawn a dummy process
        let (pid, mut child) = spawn_dummy_process();

        let config = test_box_config(false);
        let state = running_state(pid);
        runtime.box_manager.add_box(&config, &state).unwrap();

        // Drop the runtime without calling shutdown
        drop(runtime);

        // Process should be killed by Drop → shutdown_sync
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !crate::util::is_process_alive(pid),
            "Drop should trigger shutdown_sync and kill the process"
        );

        // Cleanup
        child.kill().ok();
        child.wait().ok();
    }

    #[tokio::test]
    async fn test_drop_skips_when_shutdown_already_called() {
        let (runtime, _dir) = create_test_runtime();

        // Spawn a dummy process for a detached box
        let (pid, mut child) = spawn_dummy_process();

        let config = test_box_config(true); // detached
        let state = running_state(pid);
        runtime.box_manager.add_box(&config, &state).unwrap();

        // Call shutdown explicitly (async) — skips detached boxes
        runtime.shutdown(None).await.unwrap();

        // Token is now cancelled
        assert!(runtime.shutdown_token.is_cancelled());

        // Drop should be no-op since token is already cancelled
        drop(runtime);

        // Detached process should still be alive
        assert!(
            crate::util::is_process_alive(pid),
            "Detached process should survive both shutdown() and Drop"
        );

        // Cleanup
        child.kill().ok();
        child.wait().ok();
    }

    // ====================================================================
    // shutdown() detach filter tests (async, uses in-memory cache)
    // ====================================================================

    #[tokio::test]
    async fn test_shutdown_with_no_boxes_returns_ok() {
        let (runtime, _dir) = create_test_runtime();
        let result = runtime.shutdown(None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown_then_shutdown_sync_is_noop() {
        let (runtime, _dir) = create_test_runtime();

        // Async shutdown first
        runtime.shutdown(None).await.unwrap();
        assert!(runtime.shutdown_token.is_cancelled());

        // Sync shutdown should be no-op
        runtime.shutdown_sync();
        assert!(runtime.shutdown_token.is_cancelled());
    }

    #[test]
    fn test_shutdown_sync_then_async_shutdown_is_noop() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (runtime, _dir) = create_test_runtime();

        // Sync shutdown first
        runtime.shutdown_sync();
        assert!(runtime.shutdown_token.is_cancelled());

        // Async shutdown should be no-op
        let result = rt.block_on(runtime.shutdown(None));
        assert!(result.is_ok());
    }

    // ====================================================================
    // PID file removal
    // ====================================================================

    #[test]
    fn test_shutdown_sync_removes_pid_file() {
        let (runtime, _dir) = create_test_runtime();

        let (pid, mut child) = spawn_dummy_process();

        // Use config with box_home aligned to runtime layout
        let config = test_box_config_in_layout(false, &runtime);
        let state = running_state(pid);

        // Create the box directory and PID file
        let layout = runtime
            .layout
            .box_layout(config.id.as_str(), false)
            .expect("box_layout is infallible");
        std::fs::create_dir_all(layout.root()).expect("Failed to create box directory");
        let pid_file = layout.pid_file_path();
        std::fs::write(&pid_file, pid.to_string()).expect("Failed to write PID file");
        assert!(pid_file.exists(), "PID file should exist before shutdown");

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        runtime.shutdown_sync();

        // PID file should be removed
        assert!(
            !pid_file.exists(),
            "PID file should be removed after shutdown_sync"
        );

        // Process should be dead
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!crate::util::is_process_alive(pid));

        // DB should be updated to Stopped
        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Stopped);
        assert!(db_state.pid.is_none());

        child.kill().ok();
        child.wait().ok();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_recovery_marks_unverified_live_shim_running() {
        use std::os::unix::process::CommandExt;

        let (runtime, _dir) = create_test_runtime();

        let mut child = std::process::Command::new("sleep")
            .arg0("boxlite-shim")
            .arg("300")
            .spawn()
            .expect("Failed to spawn legacy shim process");
        let pid = child.id();

        let config = test_box_config_in_layout(false, &runtime);
        let state = running_state(pid);

        let layout = runtime
            .layout
            .box_layout(config.id.as_str(), false)
            .expect("box_layout is infallible");
        std::fs::create_dir_all(layout.root()).expect("Failed to create box directory");
        let pid_file = layout.pid_file_path();
        std::fs::write(&pid_file, pid.to_string()).expect("Failed to write PID file");

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        runtime.recover_boxes().expect("Failed to recover boxes");

        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Running);
        assert_eq!(db_state.pid, Some(pid));
        assert!(
            pid_file.exists(),
            "Unverified live shim PID file should be preserved"
        );

        child.kill().ok();
        child.wait().ok();
    }

    #[test]
    fn test_remove_box_refuses_live_pid_without_force_even_unknown() {
        let (runtime, _dir) = create_test_runtime();

        let (pid, mut child) = spawn_dummy_process();
        let config = test_box_config_in_layout(false, &runtime);
        let mut state = BoxState::new();
        state.set_status(BoxStatus::Unknown);
        state.set_pid(Some(pid));
        let lock_id = runtime
            .lock_manager
            .allocate()
            .expect("Failed to allocate lock");
        state.set_lock_id(lock_id);

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let result = runtime.remove_box(&config.id, false);
        assert!(result.is_err());
        assert!(
            crate::util::is_process_alive(pid),
            "Non-force remove must not orphan or kill the live process"
        );
        assert!(
            runtime.box_manager.box_by_id(&config.id).unwrap().is_some(),
            "Box should remain in DB after rejected remove"
        );

        runtime
            .remove_box(&config.id, true)
            .expect("Force remove should kill process and remove box");
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !crate::util::is_process_alive(pid),
            "Force remove should kill the live process"
        );
        assert!(runtime.box_manager.box_by_id(&config.id).unwrap().is_none());

        child.wait().ok();
    }

    /// Write a two-line `shim.pid` whose start-time matches the live PID
    /// so the recovery and stop fingerprint checks adopt it.
    fn write_pid_file_with_fingerprint(pid_file: &std::path::Path, pid: u32) {
        if let Some(parent) = pid_file.parent() {
            std::fs::create_dir_all(parent).expect("create box directory");
        }
        let st = crate::util::process_start_time(pid).expect("OS reports start_time");
        std::fs::write(pid_file, format!("{pid}\n{st}\n")).expect("write shim.pid");
    }

    #[test]
    fn test_recovery_promotes_configured_box_with_verified_live_shim() {
        let (runtime, _dir) = create_test_runtime();
        let (pid, child) = spawn_dummy_process();
        let child = ChildGuard(child);
        let config = test_box_config_in_layout(false, &runtime);
        let state = BoxState::new();

        let layout = runtime
            .layout
            .box_layout(config.id.as_str(), false)
            .expect("box_layout is infallible");
        let pid_file = layout.pid_file_path();
        write_pid_file_with_fingerprint(&pid_file, pid);

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let (_, persisted_before) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(persisted_before.status, BoxStatus::Configured);
        assert!(persisted_before.pid.is_none());

        runtime.recover_boxes().expect("Failed to recover boxes");

        let (_, recovered) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(recovered.status, BoxStatus::Running);
        assert_eq!(recovered.pid, Some(pid));
        assert!(
            crate::util::is_process_alive(pid),
            "Recovery must adopt, not stop, the verified live shim"
        );

        drop(child);
    }

    #[tokio::test]
    async fn test_stop_kills_recovered_pid_without_live_state() {
        let (runtime, _dir) = create_test_runtime();
        let config = test_box_config_in_layout(false, &runtime);

        let (pid, mut child) = spawn_dummy_process();
        let mut state = running_state(pid);
        let lock_id = runtime
            .lock_manager
            .allocate()
            .expect("Failed to allocate lock");
        state.set_lock_id(lock_id);

        let layout = runtime
            .layout
            .box_layout(config.id.as_str(), false)
            .expect("box_layout is infallible");
        let pid_file = layout.pid_file_path();
        write_pid_file_with_fingerprint(&pid_file, pid);

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let litebox = runtime
            .get(config.id.as_str())
            .await
            .expect("Failed to get box")
            .expect("Box should exist");

        litebox.stop().await.expect("Stop should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !crate::util::is_process_alive(pid),
            "Stop should kill recovered process when no LiveState is cached"
        );
        assert!(!pid_file.exists(), "PID file should be removed on stop");

        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Stopped);
        assert!(db_state.pid.is_none());

        child.wait().ok();
    }

    #[tokio::test]
    async fn test_exec_unknown_state_returns_error_without_panic() {
        use crate::litebox::BoxCommand;

        let (runtime, _dir) = create_test_runtime();

        let config = test_box_config_in_layout(false, &runtime);
        let mut state = BoxState::new();
        state.set_status(BoxStatus::Unknown);
        let lock_id = runtime
            .lock_manager
            .allocate()
            .expect("Failed to allocate lock");
        state.set_lock_id(lock_id);

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let litebox = runtime
            .get(config.id.as_str())
            .await
            .expect("Failed to get box")
            .expect("Box should exist");

        // Refused by the status gate on `exec` (BoxStatus::can_exec), before any
        // attempt to boot a VM. It used to be refused a layer deeper, by the
        // init pipeline's plan lookup ("Cannot initialize box in Unknown
        // state"); the gate now catches it first, because a lazy boot is no
        // longer a harmless thing to attempt — it runs the box's main command.
        let result = litebox.exec(BoxCommand::new("true")).await;
        match result {
            Err(BoxliteError::InvalidState(msg)) => {
                assert!(
                    msg.contains("unknown"),
                    "the refusal must name the state that caused it, got: {msg}"
                );
            }
            Err(other) => panic!("Expected InvalidState error, got: {other}"),
            Ok(_) => panic!("Expected InvalidState error, got success"),
        }
    }

    // ====================================================================
    // Reproducer tests for follow-up fixes (see PR #492 review)
    //
    // These tests assert the *desired* behavior, not the current behavior.
    // They are written before the implementing fix so reviewers can `git
    // checkout <test commit>~1` and observe the failure mode.
    // ====================================================================

    /// REPRO: stop() must not kill a PID that does not belong to this box.
    ///
    /// Today the recovered-pid branch in `BoxImpl::stop` kills any non-
    /// `Configured|Stopped` PID without verifying the process is ours, which
    /// is a PID-reuse hazard. The fix is to call `process_identity` first.
    #[tokio::test]
    async fn test_stop_does_not_kill_unrelated_process_repro() {
        let (runtime, _dir) = create_test_runtime();

        // Spawn a plain `sleep` (NOT a boxlite-shim) — recovery should treat
        // its PID as foreign.
        let (pid, mut child) = spawn_dummy_process();
        let config = test_box_config_in_layout(false, &runtime);
        let mut state = running_state(pid);
        let lock_id = runtime
            .lock_manager
            .allocate()
            .expect("Failed to allocate lock");
        state.set_lock_id(lock_id);

        let layout = runtime
            .layout
            .box_layout(config.id.as_str(), false)
            .expect("box_layout is infallible");
        std::fs::create_dir_all(layout.root()).expect("Failed to create box directory");
        let pid_file = layout.pid_file_path();
        // Write a two-line PID file with a DELIBERATELY WRONG start_time.
        // This simulates PID reuse: the recorded fingerprint won't match
        // the OS's current reading for `pid`, so ProcessIdentity is
        // Absent and stop() must not kill the foreign process.
        let wrong_start_time = crate::util::process_start_time(pid)
            .map(|st| st.wrapping_add(1))
            .unwrap_or(u64::MAX);
        std::fs::write(&pid_file, format!("{pid}\n{wrong_start_time}\n"))
            .expect("Failed to write PID file");

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let litebox = runtime
            .get(config.id.as_str())
            .await
            .expect("Failed to get box")
            .expect("Box should exist");

        // The fix will return Ok and leave the foreign process alone.
        let _ = litebox.stop().await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            crate::util::is_process_alive(pid),
            "stop() killed a process that did not belong to this box (PID-reuse hazard)"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// REPRO: a box in `Failed` state must be retryable.
    ///
    /// `BoxStatus::can_start()` and the `Failed` doc both promise retry.
    /// Today `init_live_state` rejects `Failed` with "Cannot initialize box
    /// <id> in failed state" before reaching the restart pipeline.
    #[tokio::test]
    async fn test_failed_state_is_retryable_repro() {
        use crate::litebox::BoxCommand;

        let (runtime, _dir) = create_test_runtime();

        let config = test_box_config_in_layout(false, &runtime);
        let mut state = BoxState::new();
        state.set_status(BoxStatus::Failed);
        let lock_id = runtime
            .lock_manager
            .allocate()
            .expect("Failed to allocate lock");
        state.set_lock_id(lock_id);

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let litebox = runtime
            .get(config.id.as_str())
            .await
            .expect("Failed to get box")
            .expect("Box should exist");

        let result = litebox.exec(BoxCommand::new("true")).await;
        if let Err(BoxliteError::InvalidState(msg)) = &result {
            assert!(
                !msg.contains("in failed state"),
                "Failed-state retry was rejected by the init_live_state guard \
                 (PR #492 added this guard but Failed should still be retryable per \
                 BoxStatus::can_start). Error: {msg}"
            );
        }
        // Other errors (Engine/Internal/etc.) are fine — they mean we got past
        // the guard and into the real pipeline, which is the desired behavior.
    }

    // ====================================================================
    // Edge case: Running box with no PID
    // ====================================================================

    #[test]
    fn test_shutdown_sync_skips_running_box_with_no_pid() {
        let (runtime, _dir) = create_test_runtime();

        // Running box with pid=None (anomalous but possible after crash)
        let config = test_box_config(false);
        let mut state = BoxState::new();
        state.status = BoxStatus::Running;
        state.pid = None;

        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        // Should not panic — the `let Some(pid) = state.pid else { continue }` skips it
        runtime.shutdown_sync();

        // DB state should remain unchanged (continue skips mark_stop)
        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Running);
        assert!(db_state.pid.is_none());
    }

    // ====================================================================
    // Force-kill path (SIGTERM timeout → SIGKILL)
    // ====================================================================

    #[test]
    fn test_shutdown_sync_force_kills_stuck_process() {
        let (runtime, _dir) = create_test_runtime();

        // Spawn a process that ignores SIGTERM
        let (pid, mut child) = spawn_sigterm_ignoring_process();

        let config = test_box_config(false);
        let state = running_state(pid);
        runtime.box_manager.add_box(&config, &state).unwrap();

        let start = std::time::Instant::now();
        runtime.shutdown_sync();
        let elapsed = start.elapsed();

        // Should have waited ~5s before force killing
        assert!(
            elapsed >= std::time::Duration::from_secs(4),
            "Expected ~5s timeout before force kill, got {:?}",
            elapsed
        );

        // Process must be dead (SIGKILL)
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            !crate::util::is_process_alive(pid),
            "Process should be force-killed after SIGTERM timeout"
        );

        // DB should be updated to Stopped
        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Stopped);

        child.kill().ok();
        child.wait().ok();
    }

    // ====================================================================
    // Backend trait delegation
    // ====================================================================

    #[test]
    fn test_shutdown_sync_delegates_through_local_runtime() {
        let (runtime, _dir) = create_test_runtime();

        let (pid, mut child) = spawn_dummy_process();

        let config = test_box_config(false);
        let state = running_state(pid);
        runtime.box_manager.add_box(&config, &state).unwrap();

        // Wrap in LocalRuntime (the backend wrapper) and call via trait
        let local = LocalRuntime(Arc::clone(&runtime));
        RuntimeBackend::shutdown_sync(&local);

        // Token should be cancelled (proves delegation to RuntimeImpl)
        assert!(runtime.shutdown_token.is_cancelled());

        // Process should be dead
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!crate::util::is_process_alive(pid));

        // DB should be updated
        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Stopped);

        child.kill().ok();
        child.wait().ok();
    }

    #[tokio::test]
    async fn test_import_box_delegates_through_local_runtime() {
        let (runtime, dir) = create_test_runtime();
        let missing_archive = dir.path().join("missing.boxlite");

        let local = LocalRuntime(Arc::clone(&runtime));
        let result = RuntimeBackend::import_box(
            &local,
            BoxArchive::new(missing_archive),
            Some("imported".to_string()),
        )
        .await;

        match result {
            Err(BoxliteError::NotFound(msg)) => {
                assert!(
                    msg.contains("Archive not found"),
                    "Expected archive-not-found error, got: {msg}"
                );
            }
            _ => panic!("Expected NotFound for missing archive"),
        }
    }

    #[tokio::test]
    async fn get_or_create_rejects_nested_virtualization_upgrade() {
        let (runtime, _dir) = create_test_runtime();
        let name = Some("plain-box".to_string());

        let (_, created) = runtime
            .get_or_create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    ..Default::default()
                },
                name.clone(),
            )
            .await
            .unwrap();
        assert!(created);

        let mut nested_advanced = crate::runtime::advanced_options::AdvancedBoxOptions::default();
        nested_advanced.nested_virtualization = true;
        let result = runtime
            .get_or_create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    advanced: nested_advanced,
                    ..Default::default()
                },
                name,
            )
            .await;

        match result {
            Err(BoxliteError::Unsupported(message)) => {
                assert!(message.contains("nested virtualization"));
                assert!(message.contains("plain-box"));
            }
            Err(other) => panic!("expected Unsupported error, got: {other}"),
            Ok(_) => panic!("get_or_create must not reuse a non-nested box"),
        }
    }

    #[tokio::test]
    async fn get_or_create_rejects_privileged_upgrade() {
        let (runtime, _dir) = create_test_runtime();
        let name = Some("plain-box".to_string());

        let (_, created) = runtime
            .get_or_create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    ..Default::default()
                },
                name.clone(),
            )
            .await
            .unwrap();
        assert!(created);

        let mut advanced = crate::runtime::advanced_options::AdvancedBoxOptions::default();
        advanced.set_privileged(true);

        let result = runtime
            .get_or_create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    advanced,
                    ..Default::default()
                },
                name,
            )
            .await;

        match result {
            Err(BoxliteError::Unsupported(message)) => {
                assert!(message.contains("privileged"));
                assert!(message.contains("plain-box"));
            }
            Err(other) => panic!("expected Unsupported error, got: {other}"),
            Ok(_) => panic!("get_or_create must not reuse a non-privileged box"),
        }
    }

    #[tokio::test]
    async fn get_or_create_allows_nested_box_for_default_request() {
        let (runtime, _dir) = create_test_runtime();
        let name = Some("nested-box".to_string());

        let mut nested_advanced = crate::runtime::advanced_options::AdvancedBoxOptions::default();
        nested_advanced.nested_virtualization = true;
        let (nested_box, created) = runtime
            .get_or_create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    advanced: nested_advanced,
                    ..Default::default()
                },
                name.clone(),
            )
            .await
            .unwrap();
        assert!(created);

        let (reused_box, created) = runtime
            .get_or_create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    ..Default::default()
                },
                name,
            )
            .await
            .unwrap();

        assert!(!created);
        assert_eq!(reused_box.id(), nested_box.id());
    }

    // ====================================================================
    // Post-shutdown operation rejection
    // ====================================================================

    #[tokio::test]
    async fn test_import_after_shutdown_returns_stopped_before_archive_validation() {
        let (runtime, dir) = create_test_runtime_without_host_preflight();
        runtime.shutdown_token.cancel();

        let result = runtime
            .import_box(
                BoxArchive::new(dir.path().join("missing.boxlite")),
                Some("imported".to_string()),
            )
            .await;

        match result {
            Err(BoxliteError::Stopped(message)) => {
                assert!(message.contains("shut down"), "{message}");
            }
            Err(other) => panic!("expected Stopped before archive validation, got: {other}"),
            Ok(_) => panic!("import should fail after shutdown"),
        }
    }

    #[tokio::test]
    async fn test_create_after_shutdown_returns_stopped() {
        let (runtime, _dir) = create_test_runtime();

        // Shutdown the runtime
        runtime.shutdown(None).await.unwrap();

        // Attempt to create a box — should be rejected
        let result = runtime
            .create_inner(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    ..Default::default()
                },
                Some("test-box".into()),
                false,
            )
            .await;

        match result {
            Err(BoxliteError::Stopped(msg)) => {
                assert!(
                    msg.contains("shut down"),
                    "Error should mention 'shut down': {msg}"
                );
            }
            Err(other) => panic!("Expected Stopped error, got: {other}"),
            Ok(_) => panic!("create should fail after shutdown"),
        }
    }

    #[tokio::test]
    async fn test_pull_image_after_shutdown_returns_stopped() {
        let (runtime, _dir) = create_test_runtime();
        let local_runtime = LocalRuntime(runtime.clone());

        runtime.shutdown(None).await.unwrap();

        let result = local_runtime.pull_image("alpine:latest").await;

        match result {
            Err(BoxliteError::Stopped(msg)) => {
                assert!(
                    msg.contains("shut down"),
                    "Error should mention 'shut down': {msg}"
                );
            }
            Err(other) => panic!("Expected Stopped error, got: {other}"),
            Ok(_) => panic!("pull_image should fail after shutdown"),
        }
    }

    #[tokio::test]
    async fn test_list_images_after_shutdown_returns_stopped() {
        let (runtime, _dir) = create_test_runtime();
        let local_runtime = LocalRuntime(runtime.clone());

        runtime.shutdown(None).await.unwrap();

        let result = local_runtime.list_images().await;

        match result {
            Err(BoxliteError::Stopped(msg)) => {
                assert!(
                    msg.contains("shut down"),
                    "Error should mention 'shut down': {msg}"
                );
            }
            Err(other) => panic!("Expected Stopped error, got: {other}"),
            Ok(_) => panic!("list_images should fail after shutdown"),
        }
    }

    // ====================================================================
    // Remove box clone dependency guard (Fix #7)
    // ====================================================================

    #[test]
    fn test_remove_box_blocked_by_clone_dependency() {
        let (runtime, _dir) = create_test_runtime();

        // Create box A with a disk.
        let config_a = test_box_config_in_layout(false, &runtime);
        let state_a = BoxState::new();
        let disks_a = config_a.box_home.join("disks");
        std::fs::create_dir_all(&disks_a).unwrap();

        // Create a standalone qcow2 for box A.
        let disk_a = disks_a.join("disk.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk_a, None);

        runtime.box_manager.add_box(&config_a, &state_a).unwrap();

        // Create box B.
        let config_b = test_box_config_in_layout(false, &runtime);
        let state_b = BoxState::new();
        let disks_b = config_b.box_home.join("disks");
        std::fs::create_dir_all(&disks_b).unwrap();

        let disk_b = disks_b.join("disk.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk_b, None);

        runtime.box_manager.add_box(&config_b, &state_b).unwrap();

        // Simulate a clone dependency: create a base_disk record from A,
        // then add a ref from B to that base (B is a clone of A).
        let base_disk = crate::disk::BaseDisk {
            id: crate::BaseDiskID::parse("testB001").unwrap(),
            source_box_id: config_a.id.to_string(),
            name: None,
            kind: crate::disk::BaseDiskKind::CloneBase,
            disk_info: crate::disk::DiskInfo {
                base_path: disk_a.to_string_lossy().to_string(),
                container_disk_bytes: 0,
                size_bytes: 0,
            },
            created_at: 0,
        };
        runtime.base_disk_mgr.store().insert(&base_disk).unwrap();
        runtime
            .base_disk_mgr
            .store()
            .add_ref(
                &crate::BaseDiskID::parse("testB001").unwrap(),
                config_b.id.as_ref(),
            )
            .unwrap();

        // Try to remove box A (non-force) — should fail.
        let result = runtime.remove_box(&config_a.id, false);
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("clone dependencies"),
            "Expected dependency error, got: {msg}"
        );
        assert!(msg.contains(&config_b.id.to_string()));
    }

    #[test]
    fn test_remove_box_succeeds_when_no_dependents() {
        let (runtime, _dir) = create_test_runtime();

        let config = test_box_config_in_layout(false, &runtime);
        let state = BoxState::new();
        let disks_dir = config.box_home.join("disks");
        std::fs::create_dir_all(&disks_dir).unwrap();

        // Standalone disk (no backing).
        let disk = disks_dir.join("disk.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk, None);

        runtime.box_manager.add_box(&config, &state).unwrap();

        // The socket binding symlink must be removed with the box.
        config.sockets().ensure().unwrap();
        let binding_dir = config.sockets().binding_dir();
        assert!(std::fs::symlink_metadata(&binding_dir).is_ok());

        let result = runtime.remove_box(&config.id, false);
        assert!(result.is_ok());
        assert!(
            std::fs::symlink_metadata(&binding_dir).is_err(),
            "remove_box must unlink the socket binding symlink"
        );
    }

    #[test]
    fn test_remove_box_with_force_ignores_dependents() {
        let (runtime, _dir) = create_test_runtime();

        // Create box A.
        let config_a = test_box_config_in_layout(false, &runtime);
        let state_a = BoxState::new();
        let disks_a = config_a.box_home.join("disks");
        std::fs::create_dir_all(&disks_a).unwrap();
        let disk_a = disks_a.join("disk.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk_a, None);
        runtime.box_manager.add_box(&config_a, &state_a).unwrap();

        // Create box B.
        let config_b = test_box_config_in_layout(false, &runtime);
        let state_b = BoxState::new();
        let disks_b = config_b.box_home.join("disks");
        std::fs::create_dir_all(&disks_b).unwrap();
        let disk_b = disks_b.join("disk.qcow2");
        crate::disk::qcow2::write_test_qcow2(&disk_b, None);
        runtime.box_manager.add_box(&config_b, &state_b).unwrap();

        // Simulate clone dependency via DB refs (B depends on base from A).
        let base_disk = crate::disk::BaseDisk {
            id: crate::BaseDiskID::parse("testB002").unwrap(),
            source_box_id: config_a.id.to_string(),
            name: None,
            kind: crate::disk::BaseDiskKind::CloneBase,
            disk_info: crate::disk::DiskInfo {
                base_path: disk_a.to_string_lossy().to_string(),
                container_disk_bytes: 0,
                size_bytes: 0,
            },
            created_at: 0,
        };
        runtime.base_disk_mgr.store().insert(&base_disk).unwrap();
        runtime
            .base_disk_mgr
            .store()
            .add_ref(
                &crate::BaseDiskID::parse("testB002").unwrap(),
                config_b.id.as_ref(),
            )
            .unwrap();

        // Force remove should succeed despite dependency.
        let result = runtime.remove_box(&config_a.id, true);
        assert!(result.is_ok());
    }

    // ====================================================================
    // Stop spawn-guard: stop() on non-Running states must not spawn a VM.
    // The state.status pre-filter in stop() prevents live_state() from
    // routing through init_live_state -> BoxBuilder for Configured/Failed
    // boxes, which would otherwise trigger the full spawn pipeline.
    // ====================================================================

    #[tokio::test]
    async fn test_stop_configured_does_not_spawn() {
        let (runtime, _dir) = create_test_runtime();
        let config = test_box_config_in_layout(false, &runtime);
        let mut state = BoxState::new();
        // BoxState::new defaults to Configured
        assert_eq!(state.status, BoxStatus::Configured);
        let lock_id = runtime
            .lock_manager
            .allocate()
            .expect("Failed to allocate lock");
        state.set_lock_id(lock_id);
        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let litebox = runtime
            .get(config.id.as_str())
            .await
            .expect("get")
            .expect("Box should exist");

        let start = std::time::Instant::now();
        let _ = litebox.stop().await;
        let elapsed = start.elapsed();

        // Fast path — no spawn attempted. A real spawn takes seconds (rootfs
        // prep + libkrun init); the no-spawn cleanup path completes in
        // milliseconds. 2s is a generous upper bound.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "stop() on Configured took {:?}; likely triggered spawn pipeline",
            elapsed
        );

        // mark_stop is gated on !is_configured, so state stays Configured.
        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Configured);
    }

    #[tokio::test]
    async fn test_stop_failed_does_not_spawn() {
        let (runtime, _dir) = create_test_runtime();
        let config = test_box_config_in_layout(false, &runtime);
        let mut state = BoxState::new();
        state.mark_failed("simulated prior failure");
        assert_eq!(state.status, BoxStatus::Failed);
        let lock_id = runtime
            .lock_manager
            .allocate()
            .expect("Failed to allocate lock");
        state.set_lock_id(lock_id);
        runtime
            .box_manager
            .add_box(&config, &state)
            .expect("Failed to add box");

        let litebox = runtime
            .get(config.id.as_str())
            .await
            .expect("get")
            .expect("Box should exist");

        let start = std::time::Instant::now();
        let _ = litebox.stop().await;
        let elapsed = start.elapsed();

        // Fast path — no spawn attempted. Stop on Failed boxes should be a
        // cleanup-only operation since there is no live shim to attach.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "stop() on Failed took {:?}; likely triggered spawn pipeline",
            elapsed
        );

        // mark_stop runs for non-Configured states; Failed -> Stopped.
        let (_, db_state) = runtime.box_manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(db_state.status, BoxStatus::Stopped);
    }
}
