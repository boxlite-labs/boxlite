//! Subprocess spawning for boxlite-shim binary.

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
};

use crate::util::configure_library_env;
use crate::vmm::VmmKind;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use libkrun_sys::krun_create_ctx;

/// Spawns a subprocess with piped stdout and stderr for controlled logging.
///
/// # Arguments
/// * `binary_path` - Path to the boxlite-shim binary
/// * `engine_type` - Type of VM engine to use
/// * `config_json` - Serialized BoxConfig
///
/// # Returns
/// * `Ok(Child)` - Successfully spawned subprocess with piped stdio
/// * `Err(...)` - Failed to spawn subprocess
pub(super) fn spawn_subprocess(
    binary_path: &PathBuf,
    engine_type: VmmKind,
    config_json: &str,
) -> BoxliteResult<Child> {
    let mut cmd = Command::new(binary_path);
    cmd.arg("--engine")
        .arg(format!("{:?}", engine_type))
        .arg("--config")
        .arg(config_json);

    // Pass RUST_LOG to subprocess if set
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        cmd.env("RUST_LOG", rust_log);
    }

    // Set library search paths for bundled dependencies
    configure_library_env(&mut cmd, krun_create_ctx as *const libc::c_void);

    // Capture subprocess output for controlled logging
    // Use null for stdin to prevent libkrun from affecting parent's stdin via shared file description
    cmd.stdin(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdout(Stdio::piped());

    cmd.spawn().map_err(|e| {
        let err_msg = format!(
            "Failed to spawn VM subprocess at {}: {}",
            binary_path.display(),
            e
        );
        tracing::error!("{}", err_msg);
        BoxliteError::Engine(err_msg)
    })
}
