//! Subprocess-based Box controller management.
//!
//! This module provides the `ShimController` which manages Box lifecycle
//! by spawning `boxlite-shim` in a subprocess. The subprocess isolation
//! ensures that process takeover doesn't affect the host application.
//!
//! ## Architecture
//!
//! - **VmmController**: Spawning operations (creates VmmHandler)
//! - **VmmHandler**: Runtime operations on running VM (stop, metrics, etc.)
//!
//! This separation enables:
//! - Reconnection to existing VMs (VmmHandler::attach)
//! - Clear lifecycle boundaries (spawn vs runtime)
//! - Caller-controlled GuestSession creation

mod handler;
mod shim;
mod spawn;

use crate::vmm::InstanceSpec;
use boxlite_shared::BoxliteResult;
pub use handler::VmmHandler;
pub use shim::{ShimController, ShimHandler};

/// Raw metrics collected from Box processes.
#[derive(Clone, Debug, Default)]
pub struct VmmMetrics {
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub disk_bytes: Option<u64>,
}

/// Trait for spawning VMs.
///
/// Controllers handle the spawn/attach operation and return a VmmHandler
/// for runtime operations. The caller creates the GuestSession separately.
#[async_trait::async_trait]
pub trait VmmController: Send {
    /// Spawn a new VM and return a handler for runtime operations.
    ///
    /// # Returns
    /// - `VmmHandler`: Handler providing runtime operations (stop, metrics, etc.)
    ///
    /// # Note
    /// Caller must create GuestSession using handler.guest_transport()
    async fn start(&mut self, bundle: &InstanceSpec) -> BoxliteResult<Box<dyn VmmHandler>>;
}
