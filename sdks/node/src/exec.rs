use std::sync::Arc;

use boxlite::Execution;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use tokio::sync::Mutex;

use crate::util::map_err;

// Error messages for stream access
const ERR_STDIN_UNAVAILABLE: &str = "stdin stream not available";
const ERR_STDOUT_UNAVAILABLE: &str = "stdout stream not available";
const ERR_STDERR_UNAVAILABLE: &str = "stderr stream not available";

/// Execution result containing the exit code.
#[napi(object)]
#[derive(Clone, Debug)]
pub struct JsExecResult {
    /// Process exit code (0 = success, non-zero = error)
    pub exit_code: i32,
}

/// Stdout stream for reading command output.
///
/// Provides line-by-line access to stdout via async iteration.
#[napi]
pub struct JsExecStdout {
    pub(crate) stream: Arc<Mutex<boxlite::ExecStdout>>,
}

#[napi]
impl JsExecStdout {
    /// Read the next line from stdout.
    ///
    /// Returns null when the stream is closed (EOF).
    ///
    /// # Example
    /// ```javascript
    /// const stdout = execution.stdout();
    /// while (true) {
    ///   const line = await stdout.next();
    ///   if (line === null) break;
    ///   console.log(line);
    /// }
    /// ```
    #[napi]
    pub async fn next(&self) -> Result<Option<String>> {
        use futures::StreamExt;
        let mut guard = self.stream.lock().await;
        Ok(guard.next().await)
    }
}

/// Stderr stream for reading command error output.
///
/// Provides line-by-line access to stderr via async iteration.
#[napi]
pub struct JsExecStderr {
    pub(crate) stream: Arc<Mutex<boxlite::ExecStderr>>,
}

#[napi]
impl JsExecStderr {
    /// Read the next line from stderr.
    ///
    /// Returns null when the stream is closed (EOF).
    ///
    /// # Example
    /// ```javascript
    /// const stderr = execution.stderr();
    /// while (true) {
    ///   const line = await stderr.next();
    ///   if (line === null) break;
    ///   console.error(line);
    /// }
    /// ```
    #[napi]
    pub async fn next(&self) -> Result<Option<String>> {
        use futures::StreamExt;
        let mut guard = self.stream.lock().await;
        Ok(guard.next().await)
    }
}

/// Stdin stream for writing data to command input.
#[napi]
pub struct JsExecStdin {
    pub(crate) stream: Arc<Mutex<boxlite::ExecStdin>>,
}

#[napi]
impl JsExecStdin {
    /// Write data to stdin.
    ///
    /// # Arguments
    /// * `data` - Bytes to write (Buffer or Uint8Array)
    ///
    /// # Example
    /// ```javascript
    /// const stdin = execution.stdin();
    /// await stdin.write(Buffer.from('hello\n'));
    /// await stdin.write(new Uint8Array([10])); // newline
    /// ```
    #[napi]
    pub async fn write(&self, data: Buffer) -> Result<()> {
        let mut guard = self.stream.lock().await;
        guard.write_all(data.as_ref()).await.map_err(map_err)
    }

    /// Send a string to stdin (automatically encodes as UTF-8).
    ///
    /// # Arguments
    /// * `text` - String to write
    ///
    /// # Example
    /// ```javascript
    /// const stdin = execution.stdin();
    /// await stdin.writeString('Hello, world!\n');
    /// ```
    #[napi]
    pub async fn write_string(&self, text: String) -> Result<()> {
        self.write(text.into_bytes().into()).await
    }
}

/// Execution handle for a running command.
///
/// Provides access to stdin/stdout/stderr streams and allows waiting
/// for the command to complete.
#[napi]
pub struct JsExecution {
    pub(crate) execution: Arc<Mutex<Execution>>,
}

#[napi]
impl JsExecution {
    /// Get the execution's unique identifier.
    ///
    /// # Example
    /// ```javascript
    /// console.log(`Execution ID: ${execution.id()}`);
    /// ```
    #[napi]
    pub async fn id(&self) -> String {
        let guard = self.execution.lock().await;
        guard.id().clone()
    }

    /// Get stdin writer.
    ///
    /// Returns an error if stdin is not available (e.g., command
    /// doesn't support stdin or it was already consumed).
    ///
    /// # Example
    /// ```javascript
    /// const stdin = execution.stdin();
    /// await stdin.writeString('input data\n');
    /// ```
    #[napi]
    pub async fn stdin(&self) -> Result<JsExecStdin> {
        let mut guard = self.execution.lock().await;
        match guard.stdin() {
            Some(stream) => Ok(JsExecStdin {
                stream: Arc::new(Mutex::new(stream)),
            }),
            None => Err(Error::from_reason(ERR_STDIN_UNAVAILABLE)),
        }
    }

    /// Get stdout reader.
    ///
    /// Returns an error if stdout is not available.
    ///
    /// # Example
    /// ```javascript
    /// const stdout = execution.stdout();
    /// while (true) {
    ///   const line = await stdout.next();
    ///   if (line === null) break;
    ///   console.log(line);
    /// }
    /// ```
    #[napi]
    pub async fn stdout(&self) -> Result<JsExecStdout> {
        let mut guard = self.execution.lock().await;
        match guard.stdout() {
            Some(stream) => Ok(JsExecStdout {
                stream: Arc::new(Mutex::new(stream)),
            }),
            None => Err(Error::from_reason(ERR_STDOUT_UNAVAILABLE)),
        }
    }

    /// Get stderr reader.
    ///
    /// Returns an error if stderr is not available.
    ///
    /// # Example
    /// ```javascript
    /// const stderr = execution.stderr();
    /// while (true) {
    ///   const line = await stderr.next();
    ///   if (line === null) break;
    ///   console.error(line);
    /// }
    /// ```
    #[napi]
    pub async fn stderr(&self) -> Result<JsExecStderr> {
        let mut guard = self.execution.lock().await;
        match guard.stderr() {
            Some(stream) => Ok(JsExecStderr {
                stream: Arc::new(Mutex::new(stream)),
            }),
            None => Err(Error::from_reason(ERR_STDERR_UNAVAILABLE)),
        }
    }

    /// Wait for the command to complete.
    ///
    /// Blocks until the process exits and returns the exit code.
    ///
    /// # Returns
    /// A `Promise<JsExecResult>` with the exit code
    ///
    /// # Example
    /// ```javascript
    /// const result = await execution.wait();
    /// if (result.exitCode !== 0) {
    ///   console.error(`Command failed with exit code ${result.exitCode}`);
    /// }
    /// ```
    #[napi]
    pub async fn wait(&self) -> Result<JsExecResult> {
        let mut guard = self.execution.lock().await;
        let exec_result = guard.wait().await.map_err(map_err)?;
        Ok(JsExecResult {
            exit_code: exec_result.exit_code,
        })
    }

    /// Kill the running command (send SIGKILL).
    ///
    /// Forcefully terminates the process. Unlike stop(), this doesn't
    /// wait for graceful shutdown.
    ///
    /// # Example
    /// ```javascript
    /// await execution.kill();
    /// console.log('Command killed');
    /// ```
    #[napi]
    pub async fn kill(&self) -> Result<()> {
        let mut guard = self.execution.lock().await;
        guard.kill().await.map_err(map_err)
    }
}
