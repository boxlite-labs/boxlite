//! High-level sandbox runtime structures.

use std::sync::OnceLock;

use crate::litebox::LiteBox;
use crate::metrics::RuntimeMetrics;
use crate::runtime::options::{BoxOptions, BoxliteOptions};
use crate::runtime::rt_impl::{RuntimeImpl, SharedRuntimeImpl};
use crate::runtime::types::BoxInfo;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
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
    rt_impl: SharedRuntimeImpl,
}

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
        Ok(Self {
            rt_impl: RuntimeImpl::new(options)?,
        })
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

    // ========================================================================
    // BOX LIFECYCLE OPERATIONS (delegate to RuntimeInnerImpl)
    // ========================================================================

    /// Create a box handle.
    ///
    /// Returns immediately with a LiteBox handle. Heavy initialization (image pulling,
    /// Box startup) is deferred until the first API call on the handle.
    pub fn create(
        &self,
        image: &str,
        options: BoxOptions,
        name: Option<String>,
    ) -> BoxliteResult<LiteBox> {
        self.rt_impl.create(image, options, name)
    }

    /// Get a handle to an existing box by ID or name.
    ///
    /// The `id_or_name` parameter can be either:
    /// - A box ID (ULID format, 26 characters)
    /// - A user-defined box name
    pub fn get(&self, id_or_name: &str) -> BoxliteResult<Option<LiteBox>> {
        self.rt_impl.get(id_or_name)
    }

    /// Get information about a specific box by ID or name (without creating a handle).
    pub fn get_info(&self, id_or_name: &str) -> BoxliteResult<Option<BoxInfo>> {
        self.rt_impl.get_info(id_or_name)
    }

    /// List all boxes, sorted by creation time (newest first).
    pub fn list_info(&self) -> BoxliteResult<Vec<BoxInfo>> {
        self.rt_impl.list_info()
    }

    /// Check if a box with the given ID or name exists.
    pub fn exists(&self, id_or_name: &str) -> BoxliteResult<bool> {
        self.rt_impl.exists(id_or_name)
    }

    /// Get runtime-wide metrics.
    pub fn metrics(&self) -> RuntimeMetrics {
        self.rt_impl.metrics()
    }

    /// Remove a box completely by ID or name.
    pub async fn remove(&self, id_or_name: &str, force: bool) -> BoxliteResult<()> {
        self.rt_impl.remove(id_or_name, force)
    }
}

// ============================================================================
// RUNTIME INNER - LOCK HELPERS ONLY
// ============================================================================

impl std::fmt::Debug for BoxliteRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxliteRuntime")
            .field("home_dir", &self.rt_impl.layout.home_dir())
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
