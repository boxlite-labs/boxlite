use crate::db::{BoxStore, Database};
use crate::images::ImageManager;
use crate::init_logging_for;
use crate::litebox::config::BoxConfig;
use crate::litebox::{BoxManager, LiteBox, SharedBoxImpl};
use crate::metrics::{RuntimeMetrics, RuntimeMetricsStorage};
use crate::runtime::constants::filenames;
use crate::runtime::guest_rootfs::GuestRootfs;
use crate::runtime::layout::{FilesystemLayout, FsLayoutConfig};
use crate::runtime::lock::RuntimeLock;
use crate::runtime::options::{BoxOptions, BoxliteOptions, RootfsSpec};
use crate::runtime::types::{BoxID, BoxInfo, BoxState, BoxStatus, ContainerId, generate_box_id};
use crate::vmm::VmmKind;
use boxlite_shared::{BoxliteError, BoxliteResult, Transport};
use chrono::Utc;
use parking_lot::RwLock as ParkingRwLock;
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

    /// Runtime filesystem lock (held for lifetime). Prevent from multiple process run on same
    /// BOXLITE_HOME directory
    pub(crate) _runtime_lock: RuntimeLock,

    // ========================================================================
    // BOXLITE HANDLE CACHE: Share BoxImpl between handles
    // ========================================================================
    /// Cache of active BoxImpl instances.
    /// Uses Weak to allow automatic cleanup when all handles are dropped.
    active_boxes: ParkingRwLock<HashMap<BoxID, Weak<crate::litebox::box_impl::BoxImpl>>>,
}

/// Empty coordination lock.
///
/// Acquire this when you need atomicity across multiple operations on
/// box_manager or image_manager.
pub struct SynchronizedState;

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
                layout.home_dir().join("boxlite.db").display(),
                e
            ))
        })?;
        let box_store = BoxStore::new(db);

        let inner = Arc::new(Self {
            sync_state: RwLock::new(SynchronizedState),
            box_manager: BoxManager::new(box_store),
            image_manager,
            layout,
            guest_rootfs: Arc::new(OnceCell::new()),
            runtime_metrics: RuntimeMetricsStorage::new(),
            _runtime_lock: runtime_lock,
            active_boxes: ParkingRwLock::new(HashMap::new()),
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
    /// Returns immediately with a LiteBox handle. Heavy initialization (image pulling,
    /// Box startup) is deferred until the first API call on the handle.
    pub fn create(
        self: &Arc<Self>,
        image: &str,
        options: BoxOptions,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        // Validate name uniqueness if provided (add_box also checks, but we want early error)
        if let Some(ref name) = name
            && self.box_manager.lookup_box_id(name)?.is_some()
        {
            return Err(BoxliteError::InvalidArgument(format!(
                "box with name '{}' already exists",
                name
            )));
        }

        // Parse image reference
        let rootfs_spec = RootfsSpec::Image(image.to_string());

        // Initialize box variables with defaults
        let (config, state) = self.init_box_variables(rootfs_spec, &options, name);

        // Register in BoxManager (handles DB persistence internally)
        self.box_manager.add_box(&config, &state)?;

        // Increment boxes_created counter (lock-free!)
        self.runtime_metrics
            .boxes_created
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Create LiteBox handle with shared BoxImpl
        let box_impl = self.get_or_create_box_impl(config, state);
        Ok(LiteBox::new(box_impl))
    }

    /// Get a handle to an existing box by ID or name.
    ///
    /// Returns a LiteBox handle that can be used to operate on the box.
    /// Tries exact ID match, then name match, then ID prefix match.
    ///
    /// If another handle to the same box exists, they share the same BoxImpl
    /// (and thus the same LiveState if initialized).
    pub fn get(self: &Arc<Self>, id_or_name: &str) -> BoxliteResult<Option<LiteBox>> {
        tracing::trace!(id_or_name = %id_or_name, "RuntimeInnerImpl::get called");

        // lookup_box handles: exact ID, exact name, then ID prefix
        if let Some((config, state)) = self.box_manager.lookup_box(id_or_name)? {
            tracing::trace!(
                box_id = %config.id,
                name = ?config.name,
                "Retrieved box from manager, getting or creating BoxImpl"
            );

            let box_impl = self.get_or_create_box_impl(config, state);
            tracing::trace!(id_or_name = %id_or_name, "LiteBox created successfully");
            return Ok(Some(LiteBox::new(box_impl)));
        }

        tracing::trace!(id_or_name = %id_or_name, "Box not found in manager");
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
    pub fn get_info(&self, id_or_name: &str) -> BoxliteResult<Option<BoxInfo>> {
        // lookup_box handles: exact ID, exact name, then ID prefix
        if let Some((config, state)) = self.box_manager.lookup_box(id_or_name)? {
            return Ok(Some(BoxInfo::new(&config, &state)));
        }
        Ok(None)
    }

    /// List all boxes, sorted by creation time (newest first).
    pub fn list_info(&self) -> BoxliteResult<Vec<BoxInfo>> {
        let boxes = self.box_manager.all_boxes(true)?;
        let mut infos: Vec<_> = boxes
            .into_iter()
            .map(|(config, state)| BoxInfo::new(&config, &state))
            .collect();
        // Sort by creation time (newest first)
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(infos)
    }

    /// Check if a box with the given ID or name exists.
    pub fn exists(&self, id_or_name: &str) -> BoxliteResult<bool> {
        // lookup_box_id handles: exact ID, exact name, then ID prefix
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
    pub(crate) fn resolve_id(&self, id_or_name: &str) -> BoxliteResult<BoxID> {
        // lookup_box_id handles: exact ID, exact name, then ID prefix
        self.box_manager
            .lookup_box_id(id_or_name)?
            .ok_or_else(|| BoxliteError::NotFound(id_or_name.to_string()))
    }

    /// Remove a box from the runtime (internal implementation).
    ///
    /// This is the internal implementation called by both `BoxliteRuntime::remove()`
    /// and `LiteBox::stop()` (when `auto_remove=true`).
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

        // Get current state
        let (config, state) = self
            .box_manager
            .box_by_id(id)?
            .ok_or_else(|| BoxliteError::NotFound(id.to_string()))?;

        // Check if box is active
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

        // Invalidate cache so new handles get fresh BoxImpl
        self.invalidate_box_impl(id);

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

        tracing::info!(box_id = %id, "Removed box");
        Ok(())
    }

    // ========================================================================
    // INTERNAL - INITIALIZATION
    // ========================================================================

    /// Initialize box variables with defaults.
    fn init_box_variables(
        &self,
        rootfs_spec: RootfsSpec,
        options: &BoxOptions,
        name: Option<String>,
    ) -> (BoxConfig, BoxState) {
        use crate::litebox::config::ContainerRuntimeConfig;

        // Generate unique ID (26 chars, ULID format, sortable by time)
        let box_id = generate_box_id();

        // Generate container ID (64-char hex)
        let container_id = ContainerId::new();

        // Record creation timestamp
        let now = Utc::now();

        // Derive paths from ID (computed from layout + ID)
        let box_home = self.layout.boxes_dir().join(box_id.as_str());
        let socket_path = filenames::unix_socket_path(self.layout.home_dir(), &box_id);
        let ready_socket_path = box_home.join("sockets").join("ready.sock");

        // Create container runtime config
        let container = ContainerRuntimeConfig {
            id: container_id,
            image: rootfs_spec,
            image_config: None, // Populated during initialization
        };

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

        // Create initial state (status = Starting)
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
    /// If an active BoxImpl exists (some LiteBox handle is alive), returns a clone of its Arc.
    /// Otherwise, creates a new BoxImpl from config/state.
    fn get_or_create_box_impl(
        self: &Arc<Self>,
        config: BoxConfig,
        state: BoxState,
    ) -> SharedBoxImpl {
        use crate::litebox::box_impl::BoxImpl;

        let box_id = config.id.clone();

        // Fast path: read lock
        {
            let cache = self.active_boxes.read();
            if let Some(weak) = cache.get(&box_id)
                && let Some(strong) = weak.upgrade()
            {
                tracing::trace!(box_id = %box_id, "Reusing cached BoxImpl");
                return strong;
            }
        }

        // Slow path: write lock with double-check
        let mut cache = self.active_boxes.write();
        if let Some(weak) = cache.get(&box_id)
            && let Some(strong) = weak.upgrade()
        {
            tracing::trace!(box_id = %box_id, "Reusing cached BoxImpl (after write lock)");
            return strong;
        }

        // Create and cache
        let box_impl = Arc::new(BoxImpl::new(config, state, Arc::clone(self)));
        cache.insert(box_id.clone(), Arc::downgrade(&box_impl));
        tracing::trace!(box_id = %box_id, "Created and cached new BoxImpl");
        box_impl
    }

    /// Remove BoxImpl from cache.
    ///
    /// Called when box is stopped or removed. Existing handles become stale;
    /// new handles from runtime.get() will get a fresh BoxImpl.
    pub(crate) fn invalidate_box_impl(&self, box_id: &BoxID) {
        self.active_boxes.write().remove(box_id);
        tracing::trace!(box_id = %box_id, "Invalidated BoxImpl cache");
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
