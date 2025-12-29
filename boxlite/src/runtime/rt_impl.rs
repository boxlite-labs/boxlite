use crate::db::{BoxStore, Database};
use crate::images::ImageManager;
use crate::init_logging_for;
use crate::litebox::config::BoxConfig;
use crate::litebox::{BoxManager, LiteBox, SharedBoxImpl};
use crate::lock::{FileLockManager, LockManager};
use crate::metrics::{RuntimeMetrics, RuntimeMetricsStorage};
use crate::runtime::constants::filenames;
use crate::runtime::guest_rootfs::GuestRootfs;
use crate::runtime::layout::{FilesystemLayout, FsLayoutConfig};
use crate::runtime::lock::RuntimeLock;
use crate::runtime::options::{BoxOptions, BoxliteOptions};
use crate::runtime::types::{BoxID, BoxInfo, BoxState, BoxStatus, ContainerID};
use crate::vmm::VmmKind;
use boxlite_shared::{BoxliteError, BoxliteResult, Transport};
use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, RwLock, Weak};
use tokio::sync::OnceCell;

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
    /// Guest rootfs lazy initialization (Arc<OnceCell>)
    pub(crate) guest_rootfs: Arc<OnceCell<GuestRootfs>>,
    /// Runtime-wide metrics (AtomicU64 based, lock-free)
    pub(crate) runtime_metrics: RuntimeMetricsStorage,

    /// Per-entity lock manager for multiprocess-safe locking.
    ///
    /// Provides locks for individual entities (boxes, volumes, etc.) that work
    /// across multiple processes. Similar to Podman's lock manager.
    pub(crate) lock_manager: Arc<dyn LockManager>,

    /// Runtime filesystem lock (held for lifetime). Prevent from multiple process run on same
    /// BOXLITE_HOME directory
    pub(crate) _runtime_lock: RuntimeLock,
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

impl RuntimeImpl {
    // ========================================================================
    // CONSTRUCTION
    // ========================================================================

    /// Create a new RuntimeInnerImpl with the provided options.
    ///
    /// Performs all initialization: filesystem setup, locks, managers, and box recovery.
    pub fn new(options: BoxliteOptions) -> BoxliteResult<SharedRuntimeImpl> {
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

        init_logging_for(&layout)?;

        let runtime_lock = RuntimeLock::acquire(layout.home_dir()).map_err(|e| {
            BoxliteError::Internal(format!(
                "Failed to acquire runtime lock at {}: {}",
                layout.home_dir().display(),
                e
            ))
        })?;

        // Clean and recreate temp dir to avoid stale files from previous runs
        let _ = std::fs::remove_dir_all(layout.temp_dir());

        let image_manager = ImageManager::new(layout.images_dir()).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to initialize image manager at {}: {}",
                layout.images_dir().display(),
                e
            ))
        })?;

        let db = Database::open(&layout.db_dir().join("boxlite.db")).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to initialize database at {}: {}",
                layout.db_dir().join("boxlite.db").display(),
                e
            ))
        })?;
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

        let inner = Arc::new(Self {
            sync_state: RwLock::new(SynchronizedState {
                active_boxes_by_id: HashMap::new(),
                active_boxes_by_name: HashMap::new(),
            }),
            box_manager: BoxManager::new(box_store),
            image_manager,
            layout,
            guest_rootfs: Arc::new(OnceCell::new()),
            runtime_metrics: RuntimeMetricsStorage::new(),
            lock_manager,
            _runtime_lock: runtime_lock,
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
    /// Returns immediately with a LiteBox handle. The box is not persisted to database
    /// until first use (lazy initialization). Lock allocation and DB persistence happen
    /// in `init_live_state()` when the box is actually started.
    pub fn create(
        self: &Arc<Self>,
        options: BoxOptions,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        // Check DB for existing name
        if let Some(ref name) = name
            && self.box_manager.lookup_box_id(name)?.is_some()
        {
            return Err(BoxliteError::InvalidArgument(format!(
                "box with name '{}' already exists",
                name
            )));
        }

        // Initialize box variables with defaults (no lock, not persisted yet)
        let (config, state) = self.init_box_variables(&options, name);

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

        // DB persistence and lock allocation happen on first use (init_live_state)
        Ok(LiteBox::new(box_impl))
    }

    /// Get a handle to an existing box by ID or name.
    ///
    /// Returns a LiteBox handle that can be used to operate on the box.
    /// Checks in-memory cache first (for boxes not yet persisted), then DB.
    ///
    /// If another handle to the same box exists, they share the same BoxImpl
    /// (and thus the same LiveState if initialized).
    pub fn get(self: &Arc<Self>, id_or_name: &str) -> BoxliteResult<Option<LiteBox>> {
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
                return Ok(Some(LiteBox::new(strong)));
            }

            // Try as name
            if let Some(weak) = sync.active_boxes_by_name.get(id_or_name)
                && let Some(strong) = weak.upgrade()
            {
                tracing::trace!(name = %id_or_name, "Found box in cache by name");
                return Ok(Some(LiteBox::new(strong)));
            }
        }

        // Fall back to DB lookup (for persisted boxes)
        if let Some((config, state)) = self.box_manager.lookup_box(id_or_name)? {
            tracing::trace!(
                box_id = %config.id,
                name = ?config.name,
                "Retrieved box from DB, getting or creating BoxImpl"
            );

            let (box_impl, _) = self.get_or_create_box_impl(config, state);
            tracing::trace!(id_or_name = %id_or_name, "LiteBox created successfully");
            return Ok(Some(LiteBox::new(box_impl)));
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
    pub fn get_info(&self, id_or_name: &str) -> BoxliteResult<Option<BoxInfo>> {
        // Check in-memory cache first (for boxes created but not yet persisted)
        {
            let sync = self.sync_state.read().unwrap();

            // Try as BoxID first
            if let Some(box_id) = BoxID::parse(id_or_name)
                && let Some(weak) = sync.active_boxes_by_id.get(&box_id)
                && let Some(strong) = weak.upgrade()
            {
                return Ok(Some(strong.info()));
            }

            // Try as name
            if let Some(weak) = sync.active_boxes_by_name.get(id_or_name)
                && let Some(strong) = weak.upgrade()
            {
                return Ok(Some(strong.info()));
            }
        }

        // Fall back to DB lookup
        if let Some((config, state)) = self.box_manager.lookup_box(id_or_name)? {
            return Ok(Some(BoxInfo::new(&config, &state)));
        }
        Ok(None)
    }

    /// List all boxes, sorted by creation time (newest first).
    ///
    /// Includes both persisted boxes (from database) and in-memory boxes
    /// (created but not yet persisted).
    pub fn list_info(&self) -> BoxliteResult<Vec<BoxInfo>> {
        use std::collections::HashSet;

        // Get boxes from database
        let db_boxes = self.box_manager.all_boxes(true)?;
        let mut seen_ids: HashSet<BoxID> = db_boxes.iter().map(|(c, _)| c.id.clone()).collect();
        let mut infos: Vec<_> = db_boxes
            .into_iter()
            .map(|(config, state)| BoxInfo::new(&config, &state))
            .collect();

        // Add in-memory boxes not yet persisted
        {
            let sync = self.sync_state.read().unwrap();
            for (box_id, weak) in &sync.active_boxes_by_id {
                if !seen_ids.contains(box_id)
                    && let Some(strong) = weak.upgrade()
                {
                    infos.push(strong.info());
                    seen_ids.insert(box_id.clone());
                }
            }
        }

        // Sort by creation time (newest first)
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(infos)
    }

    /// Check if a box with the given ID or name exists.
    ///
    /// Checks in-memory cache first (for boxes not yet persisted), then database.
    pub fn exists(&self, id_or_name: &str) -> BoxliteResult<bool> {
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

        // Fall back to DB lookup
        Ok(self.box_manager.lookup_box_id(id_or_name)?.is_some())
    }

    // ========================================================================
    // PUBLIC API - METRICS
    // ========================================================================

    /// Get runtime-wide metrics.
    pub fn metrics(&self) -> RuntimeMetrics {
        RuntimeMetrics::new(self.runtime_metrics.clone())
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
    /// and `LiteBox::stop()` (when `auto_remove=true`).
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
            if state.status.is_active() {
                if force {
                    // Force mode: kill the process directly
                    if let Some(pid) = state.pid {
                        tracing::info!(box_id = %id, pid = pid, "Force killing active box");
                        crate::util::kill_process(pid);
                    }
                    // Update status to stopped and save
                    state.set_status(BoxStatus::Stopped);
                    state.set_pid(None);
                    self.box_manager.save_box(id, &state)?;
                } else {
                    // Non-force mode: error on active box
                    return Err(BoxliteError::InvalidState(format!(
                        "cannot remove active box {} (status: {:?}). Use force=true to stop first",
                        id, state.status
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

            // Delete box directory
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

            // Invalidate cache (removes from in-memory maps)
            self.invalidate_box_impl(id, box_impl.config.name.as_deref());

            // Delete box directory if it exists
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
        Err(BoxliteError::NotFound(format!("Box not found: {}", id)))
    }

    // ========================================================================
    // INTERNAL - INITIALIZATION
    // ========================================================================

    /// Initialize box variables with defaults.
    ///
    /// Creates config and state for a new box. Lock allocation and DB persistence
    /// are deferred to `init_live_state()`.
    fn init_box_variables(
        &self,
        options: &BoxOptions,
        name: Option<String>,
    ) -> (BoxConfig, BoxState) {
        use crate::litebox::config::ContainerRuntimeConfig;

        // Generate unique ID (26 chars, ULID format, sortable by time)
        let box_id = BoxID::new();

        // Generate container ID (64-char hex)
        let container_id = ContainerID::new();

        // Record creation timestamp
        let now = Utc::now();

        // Derive paths from ID (computed from layout + ID)
        let box_home = self.layout.boxes_dir().join(box_id.as_str());
        let socket_path = filenames::unix_socket_path(self.layout.home_dir(), box_id.as_str());
        let ready_socket_path = box_home.join("sockets").join("ready.sock");

        // Create container runtime config
        let container = ContainerRuntimeConfig { id: container_id };

        // Create config with defaults + user options
        let config = BoxConfig {
            id: box_id,
            name,
            created_at: now,
            container,
            options: options.clone(),
            engine_kind: VmmKind::Libkrun,
            transport: Transport::unix(socket_path),
            box_home,
            ready_socket_path,
        };

        // Create initial state (status = Starting, no lock_id yet)
        let state = BoxState::new();

        (config, state)
    }

    /// Recover boxes from persistent storage on runtime startup.
    fn recover_boxes(&self) -> BoxliteResult<()> {
        use crate::util::{is_process_alive, is_same_process};

        // Check for system reboot and reset active boxes
        self.box_manager.check_and_handle_reboot()?;

        let persisted = self.box_manager.all_boxes(true)?;

        tracing::info!("Recovering {} boxes from database", persisted.len());

        for (config, mut state) in persisted {
            let box_id = &config.id;
            let original_status = state.status;

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

            // Validate PID if present
            if let Some(pid) = state.pid {
                if is_process_alive(pid) && is_same_process(pid, box_id.as_str()) {
                    // Process is alive and it's our boxlite-shim - box stays Running
                    if state.status == BoxStatus::Running {
                        tracing::info!("Recovered box {} as Running (PID {})", box_id, pid);
                    }
                } else {
                    // Process died or PID was reused - mark as Stopped
                    if state.status.is_active() {
                        state.mark_crashed();
                        tracing::warn!(
                            "Box {} marked as Stopped (PID {} not found or different process)",
                            box_id,
                            pid
                        );
                    }
                }
            } else {
                // No PID - box was stopped gracefully or never started
                if state.status == BoxStatus::Running || state.status == BoxStatus::Starting {
                    state.set_status(BoxStatus::Stopped);
                    tracing::warn!(
                        "Box {} was Running/Starting but had no PID, marked as Stopped",
                        box_id
                    );
                }
            }

            // Save updated state to database if changed
            if state.status != original_status {
                self.box_manager.save_box(box_id, &state)?;
            }
        }

        tracing::info!("Box recovery complete");
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
        let box_impl = Arc::new(BoxImpl::new(config, state, Arc::clone(self)));
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
    pub(crate) fn acquire_write(
        &self,
    ) -> BoxliteResult<std::sync::RwLockWriteGuard<'_, SynchronizedState>> {
        self.sync_state
            .write()
            .map_err(|e| BoxliteError::Internal(format!("Coordination lock poisoned: {}", e)))
    }
}

impl std::fmt::Debug for RuntimeImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeInner")
            .field("home_dir", &self.layout.home_dir())
            .finish()
    }
}
