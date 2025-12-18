//! Vmm trait for engine-specific Box implementations.

use super::InstanceSpec;
use crate::runtime::constants::vm_defaults::{DEFAULT_CPUS, DEFAULT_MEMORY_MIB};
use boxlite_shared::errors::BoxliteResult;

/// Configuration options for creating VMM engines.
///
/// This struct contains engine-specific configuration that is needed
/// to create and initialize a VMM engine instance. Unlike BoxliteOptions
/// (which manages runtime-level paths and state), VmmConfig focuses
/// on Box-specific settings like resource limits and library locations.
#[derive(Clone, Debug)]
pub struct VmmConfig {
    /// Number of CPUs to allocate to Boxes (see vm_defaults::DEFAULT_CPUS)
    pub cpus: Option<u8>,

    /// Memory in MiB to allocate to Boxes (see vm_defaults::DEFAULT_MEMORY_MIB)
    pub memory_mib: Option<u32>,
}

impl Default for VmmConfig {
    fn default() -> Self {
        Self {
            cpus: Some(DEFAULT_CPUS),
            memory_mib: Some(DEFAULT_MEMORY_MIB),
        }
    }
}

impl VmmConfig {
    /// Create new VmmConfig with default values
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the number of CPUs
    pub fn with_cpus(mut self, cpus: u8) -> Self {
        self.cpus = Some(cpus);
        self
    }

    /// Set the memory in MiB
    pub fn with_memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }
}

/// Internal trait for engine-specific VMM instance implementations.
pub(crate) trait VmmInstanceImpl {
    /// Transfer control to the Box and run until it exits.
    fn enter(self: Box<Self>) -> BoxliteResult<()>;
}

/// A configured VMM instance ready to be executed.
///
/// VmmInstance represents a fully configured Box that has been created
/// but not yet started. Call `enter()` to transfer control to the Box.
pub struct VmmInstance {
    /// Internal engine-specific implementation
    inner: Box<dyn VmmInstanceImpl>,
}

impl VmmInstance {
    /// Create a new VmmInstance from an engine-specific implementation.
    pub(crate) fn new(inner: Box<dyn VmmInstanceImpl>) -> Self {
        Self { inner }
    }

    /// Transfer control to the Box and run until it exits.
    ///
    /// This method may never return (process takeover), depending on the engine.
    /// For some engines, this will completely hijack the calling process
    /// and transform it into the Box process.
    ///
    /// # Returns
    /// * `Ok(())` - Box exited successfully (if process takeover allows return)
    /// * `Err(...)` - Box failed to start or encountered an error
    pub fn enter(self) -> BoxliteResult<()> {
        self.inner.enter()
    }
}

/// Engine-specific VMM implementation that handles Box creation and configuration.
///
/// Vmm implementations are responsible for creating and configuring
/// Box instances with the provided configuration. This trait encapsulates
/// the engine-specific logic for Box initialization.
pub trait Vmm {
    /// Create and configure a Box instance with the given configuration.
    ///
    /// This method prepares the Box for execution but does not start it.
    /// Call `enter()` on the returned VmmInstance to transfer control.
    ///
    /// # Arguments
    /// * `config` - The Box configuration
    ///
    /// # Returns
    /// * `Ok(VmmInstance)` - Successfully created Box instance
    /// * `Err(...)` - Failed to create or configure the Box
    fn create(&mut self, config: InstanceSpec) -> BoxliteResult<VmmInstance>;
}
