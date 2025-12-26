//! High-level sandbox runtime structures.

use std::sync::{Arc, OnceLock, RwLock};

use crate::db::{BoxStore, Database};
use crate::litebox::config::BoxConfig;
use crate::litebox::{BoxManager, LiteBox};
use crate::runtime::constants::filenames;
use crate::runtime::guest_rootfs::GuestRootfs;
use crate::runtime::layout::{FilesystemLayout, FsLayoutConfig};
use crate::runtime::lock::RuntimeLock;
use crate::runtime::options::{BoxOptions, BoxliteOptions};
use crate::runtime::types::{BoxID, BoxInfo, BoxState, BoxStatus, generate_box_id};
use crate::{
    images::ImageManager,
    init_logging_for,
    metrics::{RuntimeMetrics, RuntimeMetricsStorage},
    vmm::VmmKind,
};
use boxlite_shared::{
    Transport,
    errors::{BoxliteError, BoxliteResult},
};
use chrono::Utc;
use tokio::sync::OnceCell;

// ============================================================================
// GLOBAL DEFAULT RUNTIME
// ============================================================================

/// Global default runtime singleton (lazy initialization).
///
/// This runtime uses `BoxliteOptions::default()` for configuration.
/// Most applications should use this instead of creating custom runtimes.
static DEFAULT_RUNTIME: OnceLock<BoxliteRuntime> = OnceLock::new();
// ============================================================================
// PUBLIC API
// ============================================================================

/// BoxliteRuntime provides the main entry point for creating and managing Boxes.
///
/// **Architecture**: Uses a single `RwLock` to protect all mutable state (boxes and images).
/// This eliminates nested locking and simplifies reasoning about concurrency.
///
/// **Lock Behavior**: Only one `BoxliteRuntime` can use a given `BOXLITE_HOME`
/// directory at a time. The filesystem lock is automatically released when dropped.
///
/// **Cloning**: Runtime is cheaply cloneable via `Arc` - all clones share the same state.
#[derive(Clone)]
pub struct BoxliteRuntime {
    inner: RuntimeInner,
}

/// Internal runtime state protected by single lock.
///
/// **Shared via Arc**: This is the actual shared state that can be cloned cheaply.
pub type RuntimeInner = Arc<RuntimeInnerImpl>;

/// Runtime inner implementation.
///
/// **Locking Strategy**:
/// - `sync_state`: Empty coordination lock - acquire when multi-step operations
///   on box_manager/image_manager need atomicity
/// - All managers have internal locking for individual operations
/// - Immutable fields: No lock needed - never change after creation
/// - Atomic fields: Lock-free (RuntimeMetricsStorage uses AtomicU64)
pub struct RuntimeInnerImpl {
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
    _runtime_lock: RuntimeLock,
}

/// Empty coordination lock.
///
/// Acquire this when you need atomicity across multiple operations on
/// box_manager or image_manager.
pub struct SynchronizedState;

// ============================================================================
// RUNTIME IMPLEMENTATION
// ============================================================================

impl BoxliteRuntime {
    /// Create a new BoxliteRuntime with the provided options.
    ///
    /// **Prepare Before Execute**: All setup (filesystem, locks, managers) completes
    /// before returning. No partial initialization states.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Another `BoxliteRuntime` is already using the same home directory
    /// - Filesystem initialization fails
    /// - Image API initialization fails
    pub fn new(options: BoxliteOptions) -> BoxliteResult<Self> {
        // Validate Early: Check preconditions before expensive work
        if !options.home_dir.is_absolute() {
            return Err(BoxliteError::Internal(format!(
                "home_dir must be absolute path, got: {}",
                options.home_dir.display()
            )));
        }

        // Prepare: All setup before point of no return
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

        let inner = Arc::new(RuntimeInnerImpl {
            sync_state: RwLock::new(SynchronizedState),
            box_manager: BoxManager::new(box_store),
            image_manager,
            layout,
            guest_rootfs: Arc::new(OnceCell::new()),
            runtime_metrics: RuntimeMetricsStorage::new(),
            _runtime_lock: runtime_lock,
        });

        tracing::debug!("initialized runtime");

        // Recover boxes from database
        let runtime = Self { inner };
        runtime.recover_boxes()?;

        Ok(runtime)
    }

    /// Create a new runtime with default options.
    ///
    /// This is equivalent to `BoxliteRuntime::new(BoxliteOptions::default())`
    /// but returns a `Result` instead of panicking.
    ///
    /// Prefer `default_runtime()` for most use cases (shares global instance).
    /// Use this when you need an owned, non-global runtime with default config.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boxlite::runtime::BoxliteRuntime;
    ///
    /// let runtime = BoxliteRuntime::with_defaults()?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn with_defaults() -> BoxliteResult<Self> {
        Self::new(BoxliteOptions::default())
    }

    /// Get or initialize the default global runtime.
    ///
    /// This runtime uses `BoxliteOptions::default()` for configuration.
    /// The runtime is created lazily on first access and reused for all
    /// subsequent calls.
    ///
    /// # Panics
    ///
    /// Panics if runtime initialization fails. This indicates a serious
    /// system issue (e.g., cannot create home directory, filesystem lock).
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boxlite::runtime::BoxliteRuntime;
    ///
    /// let runtime = BoxliteRuntime::default_runtime();
    /// // All subsequent calls return the same runtime
    /// let same_runtime = BoxliteRuntime::default_runtime();
    /// ```
    pub fn default_runtime() -> &'static Self {
        DEFAULT_RUNTIME.get_or_init(|| {
            Self::with_defaults().expect("Failed to initialize default BoxliteRuntime")
        })
    }

    /// Try to get the default runtime if it's been initialized.
    ///
    /// Returns `None` if the default runtime hasn't been created yet.
    /// Useful for checking if default runtime exists without creating it.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boxlite::runtime::BoxliteRuntime;
    ///
    /// if let Some(runtime) = BoxliteRuntime::try_default_runtime() {
    ///     println!("Default runtime already exists");
    /// } else {
    ///     println!("Default runtime not yet created");
    /// }
    /// ```
    pub fn try_default_runtime() -> Option<&'static Self> {
        DEFAULT_RUNTIME.get()
    }

    /// Initialize the default runtime with custom options.
    ///
    /// This must be called before the first use of `default_runtime()`.
    /// Returns an error if the default runtime has already been initialized.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Default runtime already initialized (call this early in main!)
    /// - Runtime initialization fails (filesystem, lock, etc.)
    ///
    /// # Example
    ///
    /// ```no_run
    /// use boxlite::runtime::{BoxliteRuntime, BoxliteOptions};
    /// use std::path::PathBuf;
    ///
    /// fn main() -> Result<(), Box<dyn std::error::Error>> {
    ///     let mut opts = BoxliteOptions::default();
    ///     opts.home_dir = PathBuf::from("/custom/boxlite");
    ///
    ///     BoxliteRuntime::init_default_runtime(opts)?;
    ///
    ///     // All subsequent default_runtime() calls use custom config
    ///     let runtime = BoxliteRuntime::default_runtime();
    ///     Ok(())
    /// }
    /// ```
    pub fn init_default_runtime(options: BoxliteOptions) -> BoxliteResult<()> {
        let runtime = Self::new(options)?;
        DEFAULT_RUNTIME
            .set(runtime)
            .map_err(|_| BoxliteError::Internal(
                "Default runtime already initialized. Call init_default_runtime() before any use of default_runtime().".into()
            ))
    }

    /// Create a box handle.
    ///
    /// Returns immediately with a LiteBox handle. Heavy initialization (image pulling,
    /// Box startup) is deferred until the first API call on the handle.
    ///
    /// **Single Responsibility**: Only registers box metadata and creates handle.
    /// Box startup happens separately in LiteBox::start().
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use boxlite::runtime::BoxliteRuntime;
    /// # use boxlite::runtime::options::BoxOptions;
    /// # fn example(runtime: &BoxliteRuntime) -> Result<(), Box<dyn std::error::Error>> {
    /// let litebox = runtime.create(BoxOptions::default())?;
    /// println!("Created box: {}", litebox.id());
    /// # Ok(())
    /// # }
    /// ```
    pub fn create(&self, options: BoxOptions) -> BoxliteResult<LiteBox> {
        // Stage 1: Initialize box variables with defaults
        let (config, state) = self.init_box_variables(&options);

        // Register in BoxManager (handles DB persistence internally)
        self.inner
            .box_manager
            .register(config.clone(), state.clone())?;

        // Increment boxes_created counter (lock-free!)
        self.inner
            .runtime_metrics
            .boxes_created
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Create LiteBox from config and state
        LiteBox::new(Arc::clone(&self.inner), config, &state)
    }

    /// Get a handle to an existing box.
    ///
    /// Returns a LiteBox handle that can be used to operate on the box.
    /// This follows Podman's pattern where all operations go through the object handle.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use boxlite::runtime::BoxliteRuntime;
    /// # async fn example(runtime: &BoxliteRuntime, box_id: &boxlite::BoxID) -> Result<(), Box<dyn std::error::Error>> {
    /// if let Some(litebox) = runtime.get(box_id)? {
    ///     litebox.stop().await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn get(&self, id: &BoxID) -> BoxliteResult<Option<LiteBox>> {
        tracing::trace!(box_id = %id, "BoxliteRuntime::get called");

        let Some((config, state)) = self.inner.box_manager.get(id)? else {
            tracing::trace!(box_id = %id, "Box not found in manager");
            return Ok(None);
        };

        tracing::trace!(
            box_id = %id,
            status = ?state.status,
            pid = ?state.pid,
            "Retrieved box from manager, creating LiteBox"
        );

        let litebox = LiteBox::new(Arc::clone(&self.inner), config, &state)?;

        tracing::trace!(box_id = %id, "LiteBox created successfully");
        Ok(Some(litebox))
    }

    /// Get information about a specific box (without creating a handle).
    ///
    /// Use this when you only need to read box metadata without operating on it.
    pub fn get_info(&self, id: &BoxID) -> BoxliteResult<Option<BoxInfo>> {
        self.inner.box_manager.get_info(id)
    }

    /// List all boxes, sorted by creation time (newest first).
    ///
    /// Returns metadata for all boxes without creating handles.
    /// Use `get()` to get a handle for operating on a specific box.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use boxlite::runtime::BoxliteRuntime;
    /// # fn example(runtime: &BoxliteRuntime) -> Result<(), Box<dyn std::error::Error>> {
    /// for info in runtime.list_info()? {
    ///     println!("{}: {:?}", info.id, info.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn list_info(&self) -> BoxliteResult<Vec<BoxInfo>> {
        self.inner.box_manager.list()
    }

    /// Check if a box with the given ID exists.
    ///
    /// More efficient than `get()` when you only need to check existence.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use boxlite::runtime::BoxliteRuntime;
    /// # fn example(runtime: &BoxliteRuntime, box_id: &boxlite::BoxID) -> Result<(), Box<dyn std::error::Error>> {
    /// if runtime.exists(box_id)? {
    ///     println!("Box {} exists", box_id);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn exists(&self, id: &BoxID) -> BoxliteResult<bool> {
        Ok(self.inner.box_manager.get(id)?.is_some())
    }

    /// Get runtime-wide metrics.
    ///
    /// Returns a handle for querying aggregate statistics across all boxes.
    /// All counters are monotonic and never reset.
    ///
    /// **Lock-Free**: Uses AtomicU64 internally, no lock needed!
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use boxlite_runtime::BoxliteRuntime;
    /// # fn example(runtime: &BoxliteRuntime) {
    /// let metrics = runtime.metrics();
    /// println!("Total boxes created: {}", metrics.boxes_created_total());
    /// println!("Total commands executed: {}", metrics.total_commands_executed());
    /// # }
    /// ```
    pub fn metrics(&self) -> RuntimeMetrics {
        // No lock needed! RuntimeMetricsStorage uses AtomicU64 internally
        RuntimeMetrics::new(self.inner.runtime_metrics.clone())
    }

    /// Remove a box completely.
    ///
    /// Follows Podman's `Runtime.RemoveContainer` pattern:
    /// - If `force` is true, stops the box first if running
    /// - If `force` is false, returns error if box is active
    /// - Removes box directory and all resources
    /// - Deletes from database
    ///
    /// # Arguments
    /// * `id` - Box ID to remove
    /// * `force` - If true, stop the box first; if false, error on active box
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Box doesn't exist
    /// - Box is active and `force` is false
    /// - Stop fails (when `force` is true)
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use boxlite::runtime::BoxliteRuntime;
    /// # async fn example(runtime: &BoxliteRuntime, box_id: &boxlite::BoxID) -> Result<(), Box<dyn std::error::Error>> {
    /// // Force remove (stops if running)
    /// runtime.remove(box_id, true).await?;
    ///
    /// // Non-force remove (fails if running)
    /// runtime.remove(box_id, false).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove(&self, id: &BoxID, force: bool) -> BoxliteResult<()> {
        self.inner.remove_box(id, force)
    }
}

// ============================================================================
// INTERNAL HELPERS
// ============================================================================

impl BoxliteRuntime {
    /// Initialize box variables with defaults.
    ///
    /// This is Stage 1 of box creation (following Podman's pattern):
    /// - Allocates BoxConfig and BoxState structs
    /// - Generates unique box ID (ULID format)
    /// - Sets default values (engine, paths)
    /// - Derives paths from box ID
    ///
    /// Does NOT register to database - caller must do that separately.
    ///
    /// # Arguments
    /// * `options` - User-provided options to store in config
    ///
    /// # Returns
    /// * `(BoxConfig, BoxState)` - Initialized config and state
    fn init_box_variables(&self, options: &BoxOptions) -> (BoxConfig, BoxState) {
        // Generate unique ID (26 chars, ULID format, sortable by time)
        let box_id = generate_box_id();

        // Record creation timestamp
        let now = Utc::now();

        // Derive paths from ID (computed from layout + ID)
        let box_home = self.inner.layout.boxes_dir().join(box_id.as_str());
        let socket_path = filenames::unix_socket_path(self.inner.layout.home_dir(), &box_id);
        let ready_socket_path = box_home.join("sockets").join("ready.sock");

        // Create config with defaults + user options
        let config = BoxConfig {
            id: box_id,
            created_at: now,
            options: options.clone(),
            engine_kind: VmmKind::Libkrun, // Default engine
            transport: Transport::unix(socket_path),
            box_home,
            ready_socket_path,
        };

        // Create initial state (status = Starting)
        let state = BoxState::new();

        (config, state)
    }

    /// Recover boxes from persistent storage on runtime startup.
    ///
    /// Loads all boxes from the database, validates PIDs, and updates states:
    /// - Running: PID is valid and process is running (can reattach)
    /// - Stopped: PID was set but process is dead, or no PID was set
    fn recover_boxes(&self) -> BoxliteResult<()> {
        use crate::util::{is_process_alive, is_same_process};

        // Check for system reboot and reset active boxes
        self.inner.box_manager.check_and_handle_reboot()?;

        let persisted = self.inner.box_manager.load_all_persisted()?;

        tracing::info!("Recovering {} boxes from database", persisted.len());

        for (config, mut state) in persisted {
            let box_id = &config.id;

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
                        state.mark_crashed(); // This sets status to Stopped
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
                    // This shouldn't happen, but handle it gracefully
                    state.set_status(BoxStatus::Stopped);
                    tracing::warn!(
                        "Box {} was Running/Starting but had no PID, marked as Stopped",
                        box_id
                    );
                }
            }

            // Register recovered box in memory cache
            self.inner.box_manager.register_recovered(config, state)?;
        }

        tracing::info!("Box recovery complete");
        Ok(())
    }
}

// ============================================================================
// RUNTIME INNER - LOCK HELPERS ONLY
// ============================================================================

impl RuntimeInnerImpl {
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
            .get(id)?
            .ok_or_else(|| BoxliteError::NotFound(id.to_string()))?;

        // Check if box is active
        if state.status.is_active() {
            if force {
                // Force mode: kill the process directly
                if let Some(pid) = state.pid {
                    tracing::info!(box_id = %id, pid = pid, "Force killing active box");
                    crate::util::kill_process(pid);
                }
                // Update status to stopped
                self.box_manager.update_status(id, BoxStatus::Stopped)?;
                self.box_manager.update_pid(id, None)?;
            } else {
                // Non-force mode: error on active box
                return Err(BoxliteError::InvalidState(format!(
                    "cannot remove active box {} (status: {:?}). Use force=true to stop first",
                    id, state.status
                )));
            }
        }

        // Remove from BoxManager (database-first)
        self.box_manager.remove(id)?;

        // Delete box directory
        let box_home = config.box_home;
        if box_home.exists() {
            if let Err(e) = std::fs::remove_dir_all(&box_home) {
                tracing::warn!(
                    box_id = %id,
                    path = %box_home.display(),
                    error = %e,
                    "Failed to cleanup box directory"
                );
            }
        }

        tracing::info!(box_id = %id, "Removed box");
        Ok(())
    }
}

impl std::fmt::Debug for BoxliteRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxliteRuntime")
            .field("home_dir", &self.inner.layout.home_dir())
            .finish()
    }
}

impl std::fmt::Debug for RuntimeInnerImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeInner")
            .field("home_dir", &self.layout.home_dir())
            .finish()
    }
}

// ============================================================================
// THREAD SAFETY ASSERTIONS
// ============================================================================

// Compile-time assertions to ensure BoxliteRuntime is Send + Sync
// This is critical for multithreaded usage (e.g., Python GIL release)
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<BoxliteRuntime>;
};
