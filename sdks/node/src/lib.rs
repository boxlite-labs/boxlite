#![deny(clippy::all)]

//! BoxLite Node.js bindings.
//!
//! This crate provides napi-rs bindings for BoxLite, allowing JavaScript/TypeScript
//! applications to create and manage isolated VM-based containers.

mod box_handle;
mod exec;
mod info;
mod metrics;
mod options;
mod runtime;
mod util;

// Re-export all public types
pub use box_handle::JsBox;
pub use exec::{JsExecResult, JsExecStderr, JsExecStdin, JsExecStdout, JsExecution};
pub use info::JsBoxInfo;
pub use metrics::{JsBoxMetrics, JsRuntimeMetrics};
pub use options::{JsBoxOptions, JsEnvVar, JsOptions, JsPortSpec, JsVolumeSpec};
pub use runtime::JsBoxlite;
