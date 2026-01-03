use std::sync::Arc;

use boxlite::BoxliteRuntime;
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::box_handle::JsBox;
use crate::info::JsBoxInfo;
use crate::metrics::JsRuntimeMetrics;
use crate::options::{JsBoxOptions, JsOptions};
use crate::util::map_err;

/// BoxLite runtime instance.
///
/// The main entry point for creating and managing boxes. Each runtime
/// instance manages a separate data directory with its own boxes, images,
/// and configuration.
#[napi]
pub struct JsBoxlite {
    runtime: Arc<BoxliteRuntime>,
}

#[napi]
impl JsBoxlite {
    /// Create a new runtime with custom options.
    ///
    /// # Arguments
    /// * `options` - Runtime configuration (e.g., custom home directory)
    ///
    /// # Example
    /// ```javascript
    /// const runtime = new Boxlite({ homeDir: '/custom/path' });
    /// ```
    #[napi(constructor)]
    pub fn new(options: JsOptions) -> Result<Self> {
        let runtime = BoxliteRuntime::new(options.into()).map_err(map_err)?;

        Ok(Self {
            runtime: Arc::new(runtime),
        })
    }

    /// Get the default runtime instance.
    ///
    /// Uses ~/.boxlite as the home directory. This is the recommended
    /// way to get a runtime for most use cases.
    ///
    /// # Example
    /// ```javascript
    /// const runtime = Boxlite.withDefaultConfig();
    /// ```
    #[napi(factory)]
    pub fn with_default_config() -> Result<Self> {
        let runtime = BoxliteRuntime::default_runtime();
        Ok(Self {
            runtime: Arc::new(runtime.clone()),
        })
    }

    /// Initialize the default runtime with custom options.
    ///
    /// This must be called before any calls to `Boxlite.withDefaultConfig()` if you
    /// want to customize the default runtime's configuration.
    ///
    /// # Arguments
    /// * `options` - Runtime configuration
    ///
    /// # Example
    /// ```javascript
    /// Boxlite.initDefault({ homeDir: '/custom/path' });
    /// const runtime = Boxlite.withDefaultConfig(); // Uses /custom/path
    /// ```
    #[napi]
    pub fn init_default(options: JsOptions) -> Result<()> {
        BoxliteRuntime::init_default_runtime(options.into()).map_err(map_err)
    }

    /// Create a new box.
    ///
    /// This asynchronously pulls the container image (if needed), prepares
    /// the rootfs, spawns the VM, and waits for the guest agent to be ready.
    ///
    /// # Arguments
    /// * `options` - Box configuration (image, resources, volumes, etc.)
    /// * `name` - Optional user-defined name for the box
    ///
    /// # Returns
    /// A `Promise<JsBox>` that resolves to a box handle
    ///
    /// # Example
    /// ```javascript
    /// const box = runtime.create({
    ///   image: 'python:slim',
    ///   memoryMib: 512,
    ///   cpus: 2
    /// }, 'my-python-box');
    /// ```
    #[napi]
    pub fn create(&self, options: JsBoxOptions, name: Option<String>) -> Result<JsBox> {
        let handle = self.runtime.create(options.into(), name).map_err(map_err)?;

        Ok(JsBox {
            handle: Arc::new(handle),
        })
    }

    /// List all boxes managed by this runtime.
    ///
    /// Returns metadata for all boxes, including stopped and failed boxes.
    ///
    /// # Returns
    /// Array of box information objects
    ///
    /// # Example
    /// ```javascript
    /// const boxes = runtime.listInfo();
    /// boxes.forEach(box => {
    ///   console.log(`${box.id}: ${box.status}`);
    /// });
    /// ```
    #[napi]
    pub fn list_info(&self) -> Result<Vec<JsBoxInfo>> {
        let infos = self.runtime.list_info().map_err(map_err)?;

        Ok(infos.into_iter().map(JsBoxInfo::from).collect())
    }

    /// Get information about a specific box by ID or name.
    ///
    /// # Arguments
    /// * `id_or_name` - Either a box ID (ULID) or user-defined name
    ///
    /// # Returns
    /// Box information if found, null otherwise
    ///
    /// # Example
    /// ```javascript
    /// const info = runtime.getInfo('my-python-box');
    /// if (info) {
    ///   console.log(`Status: ${info.status}`);
    /// }
    /// ```
    #[napi]
    pub fn get_info(&self, id_or_name: String) -> Result<Option<JsBoxInfo>> {
        Ok(self
            .runtime
            .get_info(&id_or_name)
            .map_err(map_err)?
            .map(JsBoxInfo::from))
    }

    /// Get a box handle by ID or name (for reattach or restart).
    ///
    /// This allows you to reconnect to a box that was created in a previous
    /// session or by another process.
    ///
    /// # Arguments
    /// * `id_or_name` - Either a box ID (ULID) or user-defined name
    ///
    /// # Returns
    /// Box handle if found, null otherwise
    ///
    /// # Example
    /// ```javascript
    /// const box = runtime.get('my-python-box');
    /// if (box) {
    ///   await box.exec('python', ['--version']);
    /// }
    /// ```
    #[napi]
    pub fn get(&self, id_or_name: String) -> Result<Option<JsBox>> {
        tracing::trace!("JsBoxlite.get() called with id_or_name={}", id_or_name);

        let result = self.runtime.get(&id_or_name).map_err(map_err)?;

        tracing::trace!("Rust get() returned: is_some={}", result.is_some());

        let js_box = result.map(|handle| {
            tracing::trace!("Wrapping LiteBox in JsBox for id_or_name={}", id_or_name);
            JsBox {
                handle: Arc::new(handle),
            }
        });

        tracing::trace!(
            "Returning JsBox to JavaScript: is_some={}",
            js_box.is_some()
        );
        Ok(js_box)
    }

    /// Get runtime metrics.
    ///
    /// Returns aggregated statistics about all boxes managed by this runtime.
    ///
    /// # Example
    /// ```javascript
    /// const metrics = runtime.metrics();
    /// console.log(`Boxes created: ${metrics.boxesCreatedTotal}`);
    /// console.log(`Running: ${metrics.numRunningBoxes}`);
    /// ```
    #[napi]
    pub fn metrics(&self) -> JsRuntimeMetrics {
        JsRuntimeMetrics::from(self.runtime.metrics())
    }

    /// Remove a box by ID or name.
    ///
    /// This stops the box (if running) and deletes all associated files
    /// (rootfs, disk, configuration).
    ///
    /// # Arguments
    /// * `id_or_name` - Either a box ID (ULID) or user-defined name
    /// * `force` - If true, stop the box first if running (default: false)
    ///
    /// # Example
    /// ```javascript
    /// await runtime.remove('my-python-box', true);
    /// ```
    #[napi]
    pub async fn remove(&self, id_or_name: String, force: Option<bool>) -> Result<()> {
        let runtime = Arc::clone(&self.runtime);
        runtime
            .remove(&id_or_name, force.unwrap_or(false))
            .await
            .map_err(map_err)
    }

    /// Close the runtime (no-op, provided for API compatibility).
    ///
    /// BoxLite doesn't require explicit cleanup, but this method is provided
    /// for consistency with other SDKs.
    #[napi]
    pub fn close(&self) -> Result<()> {
        Ok(())
    }
}
