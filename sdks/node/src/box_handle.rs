use std::sync::Arc;

use boxlite::{BoxCommand, LiteBox};
use napi::bindgen_prelude::*;
use napi_derive::napi;

use crate::exec::JsExecution;
use crate::info::JsBoxInfo;
use crate::metrics::JsBoxMetrics;
use crate::util::map_err;

/// Box handle for interacting with a running container.
///
/// Provides methods to execute commands, get status, and stop the box.
/// Each box runs in an isolated VM with its own rootfs and resources.
#[napi]
pub struct JsBox {
    pub(crate) handle: Arc<LiteBox>,
}

#[napi]
impl JsBox {
    /// Get the box's unique identifier (ULID).
    ///
    /// # Example
    /// ```javascript
    /// console.log(`Box ID: ${box.id}`);
    /// ```
    #[napi(getter)]
    pub fn id(&self) -> String {
        self.handle.id().to_string()
    }

    /// Get the box's user-defined name (if set).
    ///
    /// # Example
    /// ```javascript
    /// if (box.name) {
    ///   console.log(`Box name: ${box.name}`);
    /// }
    /// ```
    #[napi(getter)]
    pub fn name(&self) -> Option<String> {
        self.handle.name().map(|s| s.to_string())
    }

    /// Get box metadata (synchronous).
    ///
    /// Returns current status, timestamps, and other metadata without
    /// making any async calls.
    ///
    /// # Example
    /// ```javascript
    /// const info = box.info();
    /// console.log(`Status: ${info.status}`);
    /// console.log(`Created: ${info.createdAt}`);
    /// ```
    #[napi]
    pub fn info(&self) -> JsBoxInfo {
        JsBoxInfo::from(self.handle.info())
    }

    /// Execute a command inside the box.
    ///
    /// Returns an execution handle that provides access to stdin/stdout/stderr
    /// streams and allows waiting for completion.
    ///
    /// # Arguments
    /// * `command` - Command to execute (path or name)
    /// * `args` - Command arguments (optional)
    /// * `env` - Environment variables as array of [key, value] tuples (optional)
    /// * `tty` - Enable TTY mode for interactive programs (optional, default: false)
    ///
    /// # Returns
    /// A `Promise<JsExecution>` that resolves to an execution handle
    ///
    /// # Example
    /// ```javascript
    /// // Simple command
    /// const exec = await box.exec('ls', ['-la', '/']);
    ///
    /// // With environment variables
    /// const exec = await box.exec('python', ['-c', 'print("hello")'], [
    ///   ['PYTHONPATH', '/custom/path']
    /// ]);
    ///
    /// // Interactive TTY
    /// const exec = await box.exec('bash', [], [], true);
    /// ```
    #[napi]
    pub async fn exec(
        &self,
        command: String,
        args: Option<Vec<String>>,
        env: Option<Vec<Vec<String>>>,
        tty: Option<bool>,
    ) -> Result<JsExecution> {
        let handle = Arc::clone(&self.handle);

        let args = args.unwrap_or_default();
        let tty = tty.unwrap_or(false);

        // Build command
        let mut cmd = BoxCommand::new(command);
        cmd = cmd.args(args);

        // Add environment variables if provided
        if let Some(env_vars) = env {
            for env_var in env_vars {
                if env_var.len() == 2 {
                    cmd = cmd.env(env_var[0].clone(), env_var[1].clone());
                }
            }
        }

        if tty {
            cmd = cmd.tty(true);
        }

        let execution = handle.exec(cmd).await.map_err(map_err)?;

        Ok(JsExecution {
            execution: Arc::new(tokio::sync::Mutex::new(execution)),
        })
    }

    /// Stop the box (preserves state for restart).
    ///
    /// Sends a graceful shutdown signal to the VM. The box's rootfs and
    /// configuration are preserved, allowing it to be restarted later
    /// with `runtime.get(box_id)` (if auto_remove is false).
    ///
    /// # Example
    /// ```javascript
    /// await box.stop();
    /// console.log('Box stopped');
    /// ```
    #[napi]
    pub async fn stop(&self) -> Result<()> {
        self.handle.stop().await.map_err(map_err)
    }

    /// Get box metrics.
    ///
    /// Returns detailed resource usage and performance metrics including
    /// CPU, memory, network stats, and lifecycle timing.
    ///
    /// # Returns
    /// A `Promise<JsBoxMetrics>` with current metrics
    ///
    /// # Example
    /// ```javascript
    /// const metrics = await box.metrics();
    /// console.log(`CPU: ${metrics.cpuPercent}%`);
    /// console.log(`Memory: ${metrics.memoryBytes} bytes`);
    /// console.log(`Commands executed: ${metrics.commandsExecutedTotal}`);
    /// ```
    #[napi]
    pub async fn metrics(&self) -> Result<JsBoxMetrics> {
        let metrics = self.handle.metrics().await.map_err(map_err)?;
        Ok(JsBoxMetrics::from(metrics))
    }
}
