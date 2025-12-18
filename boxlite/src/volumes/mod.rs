//! Storage operations (disk image management).
//!
//! Provides disk image creation and management for Box block devices.

mod block_device;
pub mod constants;
mod disk;
pub(crate) mod ext4;
mod qcow2;

pub use block_device::BlockDeviceManager;
pub use disk::{Disk, DiskFormat};
pub use ext4::create_ext4_from_dir;
pub use qcow2::{BackingFormat, Qcow2Helper};
