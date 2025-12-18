//! Storage operations (volume mounting).
//!
//! Provides unified abstraction for mounting different volume types:
//! - Virtiofs: Host-shared directories via virtio-fs
//! - Block devices: Disk images attached via virtio-blk

mod block_device;
#[allow(dead_code)]
mod copy;
mod perms;
mod virtiofs;
mod volume;

pub use volume::mount_volumes;
