//! C FFI bindings for BoxLite
//!
//! This module provides a C-compatible API for integrating BoxLite into C/C++ applications.
//! The API uses JSON for complex types to avoid ABI compatibility issues.
//!
//! # Safety
//!
//! All functions in this module are unsafe because they:
//! - Dereference raw pointers passed from C
//! - Require the caller to ensure pointer validity and proper cleanup
//! - May write to caller-provided output pointers

#![allow(unsafe_op_in_unsafe_fn)]
#![allow(clippy::missing_safety_doc)]
#![allow(clippy::doc_overindented_list_items)]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;

use tokio::runtime::Runtime as TokioRuntime;

use boxlite::BoxID;
use boxlite::litebox::LiteBox;
use boxlite::runtime::BoxliteRuntime;
use boxlite::runtime::options::{BoxOptions, BoxliteOptions};
use boxlite_shared::errors::BoxliteError;

/// Opaque handle to a BoxliteRuntime instance
pub struct CBoxliteRuntime {
    runtime: BoxliteRuntime,
    tokio_rt: Arc<TokioRuntime>,
}

/// Opaque handle to a running box
pub struct CBoxHandle {
    handle: LiteBox,
    #[allow(dead_code)]
    box_id: BoxID,
    tokio_rt: Arc<TokioRuntime>,
}

/// Helper to convert Rust error to C string
fn error_to_c_string(err: BoxliteError) -> *mut c_char {
    let msg = format!("{}", err);
    match CString::new(msg) {
        Ok(s) => s.into_raw(),
        Err(_) => {
            let fallback = CString::new("Failed to format error message").unwrap();
            fallback.into_raw()
        }
    }
}

/// Helper to convert C string to Rust string
unsafe fn c_str_to_string(s: *const c_char) -> Result<String, BoxliteError> {
    if s.is_null() {
        return Err(BoxliteError::Internal("null pointer".to_string()));
    }
    unsafe {
        CStr::from_ptr(s)
            .to_str()
            .map(|s| s.to_string())
            .map_err(|e| BoxliteError::Internal(format!("invalid UTF-8: {}", e)))
    }
}

/// Get BoxLite version string
///
/// # Returns
/// Static string containing the version (e.g., "0.1.0")
#[unsafe(no_mangle)]
pub extern "C" fn boxlite_version() -> *const c_char {
    // Static string, safe to return pointer
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Create a new BoxLite runtime
///
/// # Arguments
/// * `home_dir` - Path to BoxLite home directory (stores images, rootfs, etc.)
///                If NULL, uses default: ~/.boxlite
/// * `out_error` - Output parameter for error message (caller must free with boxlite_free_string)
///
/// # Returns
/// Pointer to CBoxliteRuntime on success, NULL on failure
///
/// # Example
/// ```c
/// char *error = NULL;
/// BoxliteRuntime *runtime = boxlite_runtime_new("/tmp/boxlite", &error);
/// if (!runtime) {
///     fprintf(stderr, "Error: %s\n", error);
///     boxlite_free_string(error);
///     return 1;
/// }
/// ```
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_runtime_new(
    home_dir: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut CBoxliteRuntime {
    // Create tokio runtime
    let tokio_rt = match TokioRuntime::new() {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(BoxliteError::Internal(format!(
                    "Failed to create async runtime: {}",
                    e
                )));
            }
            return ptr::null_mut();
        }
    };

    // Parse options
    let mut options = BoxliteOptions::default();
    if !home_dir.is_null() {
        match c_str_to_string(home_dir) {
            Ok(path) => options.home_dir = path.into(),
            Err(e) => {
                if !out_error.is_null() {
                    *out_error = error_to_c_string(e);
                }
                return ptr::null_mut();
            }
        }
    }

    // Create runtime
    let runtime = match BoxliteRuntime::new(options) {
        Ok(rt) => rt,
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(e);
            }
            return ptr::null_mut();
        }
    };

    Box::into_raw(Box::new(CBoxliteRuntime { runtime, tokio_rt }))
}

/// Create a new box with the given options (JSON)
///
/// # Arguments
/// * `runtime` - BoxLite runtime instance
/// * `options_json` - JSON-encoded BoxOptions, e.g.:
///                    `{"images": {"Reference": "alpine:3.19"}, "working_dir": "/workspace"}`
/// * `out_error` - Output parameter for error message
///
/// # Returns
/// Pointer to CBoxHandle on success, NULL on failure
///
/// # Example
/// ```c
/// const char *opts = "{\"images\":{\"Reference\":\"alpine:3.19\"}}";
/// BoxHandle *box = boxlite_create_box(runtime, image, opts, &error);
/// ```
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_create_box(
    runtime: *mut CBoxliteRuntime,
    image: *const c_char,
    options_json: *const c_char,
    out_error: *mut *mut c_char,
) -> *mut CBoxHandle {
    if runtime.is_null() {
        if !out_error.is_null() {
            *out_error = error_to_c_string(BoxliteError::Internal("runtime is null".to_string()));
        }
        return ptr::null_mut();
    }

    let runtime_ref = &mut *runtime;

    // Parse image string
    let image_str = match c_str_to_string(image) {
        Ok(s) => s,
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(e);
            }
            return ptr::null_mut();
        }
    };

    // Parse JSON options
    let options_str = match c_str_to_string(options_json) {
        Ok(s) => s,
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(e);
            }
            return ptr::null_mut();
        }
    };

    let options: BoxOptions = match serde_json::from_str(&options_str) {
        Ok(opts) => opts,
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(BoxliteError::Internal(format!(
                    "Invalid JSON options: {}",
                    e
                )));
            }
            return ptr::null_mut();
        }
    };

    // Create box (no name support in C API yet)
    let result = runtime_ref.runtime.create(&image_str, options, None);

    match result {
        Ok(handle) => {
            let box_id = handle.id().clone();
            Box::into_raw(Box::new(CBoxHandle {
                handle,
                box_id,
                tokio_rt: runtime_ref.tokio_rt.clone(),
            }))
        }
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(e);
            }
            ptr::null_mut()
        }
    }
}

/// Execute a command in a box
///
/// # Arguments
/// * `handle` - Box handle
/// * `command` - Command to execute
/// * `args_json` - JSON array of arguments, e.g.: `["arg1", "arg2"]`
/// * `callback` - Optional callback for streaming output (chunk_text, is_stderr, user_data)
/// * `user_data` - User data passed to callback
/// * `out_error` - Output parameter for error message
///
/// # Returns
/// Exit code on success, -1 on failure
///
/// # Example
/// ```c
/// const char *args = "[\"hello\"]";
/// int exit_code = boxlite_execute(box, "echo", args, NULL, NULL, &error);
/// ```
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_execute(
    handle: *mut CBoxHandle,
    command: *const c_char,
    args_json: *const c_char,
    callback: Option<extern "C" fn(*const c_char, c_int, *mut c_void)>,
    user_data: *mut c_void,
    out_error: *mut *mut c_char,
) -> c_int {
    if handle.is_null() {
        if !out_error.is_null() {
            *out_error = error_to_c_string(BoxliteError::Internal("handle is null".into()));
        }
        return -1;
    }

    let handle_ref = &mut *handle;

    // Parse command
    let cmd_str = match c_str_to_string(command) {
        Ok(s) => s,
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(e);
            }
            return -1;
        }
    };

    // Parse args
    let args: Vec<String> = if !args_json.is_null() {
        match c_str_to_string(args_json) {
            Ok(json_str) => match serde_json::from_str(&json_str) {
                Ok(a) => a,
                Err(e) => {
                    if !out_error.is_null() {
                        *out_error = error_to_c_string(BoxliteError::Internal(format!(
                            "Invalid args JSON: {}",
                            e
                        )));
                    }
                    return -1;
                }
            },
            Err(e) => {
                if !out_error.is_null() {
                    *out_error = error_to_c_string(e);
                }
                return -1;
            }
        }
    } else {
        vec![]
    };

    let mut cmd = boxlite::BoxCommand::new(cmd_str);
    cmd = cmd.args(args);

    // Execute command using new API
    let result = handle_ref.tokio_rt.block_on(async {
        let mut execution = handle_ref.handle.exec(cmd).await?;

        // Stream output to callback if provided
        if let Some(cb) = callback {
            use futures::StreamExt;

            // Take stdout and stderr
            let mut stdout = execution.stdout();
            let mut stderr = execution.stderr();

            // Read both streams
            loop {
                tokio::select! {
                    Some(line) = async {
                        match &mut stdout {
                            Some(s) => s.next().await,
                            None => None,
                        }
                    } => {
                        let c_text = CString::new(line).unwrap_or_default();
                        cb(c_text.as_ptr(), 0, user_data); // 0 = stdout
                    }
                    Some(line) = async {
                        match &mut stderr {
                            Some(s) => s.next().await,
                            None => None,
                        }
                    } => {
                        let c_text = CString::new(line).unwrap_or_default();
                        cb(c_text.as_ptr(), 1, user_data); // 1 = stderr
                    }
                    else => break,
                }
            }
        }

        // Wait for execution to complete
        let status = execution.wait().await?;
        Ok::<i32, BoxliteError>(status.exit_code)
    });

    match result {
        Ok(exit_code) => exit_code,
        Err(e) => {
            if !out_error.is_null() {
                *out_error = error_to_c_string(e);
            }
            -1
        }
    }
}

/// Stop a box
///
/// # Arguments
/// * `handle` - Box handle (will be consumed/freed)
/// * `out_error` - Output parameter for error message
///
/// # Returns
/// 0 on success, -1 on failure
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_stop_box(
    handle: *mut CBoxHandle,
    out_error: *mut *mut c_char,
) -> c_int {
    if handle.is_null() {
        if !out_error.is_null() {
            unsafe {
                *out_error = error_to_c_string(BoxliteError::Internal("handle is null".into()));
            }
        }
        return -1;
    }

    let handle_box = unsafe { Box::from_raw(handle) };

    // Block on async stop using the stored tokio runtime
    let result = handle_box.tokio_rt.block_on(handle_box.handle.stop());

    match result {
        Ok(_) => 0,
        Err(e) => {
            if !out_error.is_null() {
                unsafe {
                    *out_error = error_to_c_string(e);
                }
            }
            -1
        }
    }
}

/// Free a runtime instance
///
/// # Arguments
/// * `runtime` - Runtime instance to free (can be NULL)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_runtime_free(runtime: *mut CBoxliteRuntime) {
    if !runtime.is_null() {
        unsafe {
            drop(Box::from_raw(runtime));
        }
    }
}

/// Free a string allocated by BoxLite
///
/// # Arguments
/// * `str` - String to free (can be NULL)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn boxlite_free_string(str: *mut c_char) {
    if !str.is_null() {
        unsafe {
            drop(CString::from_raw(str));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        unsafe {
            let version = CStr::from_ptr(boxlite_version()).to_str().unwrap();
            assert!(!version.is_empty());
            assert!(version.contains('.'));
        }
    }
}
