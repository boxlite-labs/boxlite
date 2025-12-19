//! Container volume management.
//!
//! Manages bind mounts from guest VM paths into container namespace.
//! Works with GuestVolumeManager to set up the underlying virtiofs shares.

use std::path::PathBuf;

use super::guest_volume::GuestVolumeManager;

/// Container bind mount entry.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ContainerMount {
    /// Source path in guest VM
    pub source: String,
    /// Destination path in container
    pub destination: String,
    /// Read-only mount
    pub read_only: bool,
}

/// Manages container-level volume configuration.
///
/// Holds a reference to GuestVolumeManager and tracks bind mounts
/// from guest VM paths into container namespace.
#[allow(dead_code)]
pub struct ContainerVolumeManager<'a> {
    guest: &'a mut GuestVolumeManager,
    container_mounts: Vec<ContainerMount>,
}

#[allow(dead_code)]
impl<'a> ContainerVolumeManager<'a> {
    /// Create a new container volume manager.
    pub fn new(guest: &'a mut GuestVolumeManager) -> Self {
        Self {
            guest,
            container_mounts: Vec::new(),
        }
    }

    /// Add a volume visible to both guest and container.
    ///
    /// Sets up virtiofs share in guest, records bind mount for container.
    pub fn add_volume(
        &mut self,
        host_path: PathBuf,
        guest_path: &str,
        container_path: &str,
        read_only: bool,
    ) {
        // Add virtiofs share to guest
        let tag = self.guest.next_auto_tag();
        self.guest
            .add_fs_share(&tag, host_path, guest_path, read_only);

        // Record container bind mount
        self.container_mounts.push(ContainerMount {
            source: guest_path.to_string(),
            destination: container_path.to_string(),
            read_only,
        });
    }

    /// Add a container bind mount directly.
    ///
    /// Use when guest path already exists (e.g., from block device mount).
    pub fn add_bind(&mut self, guest_path: &str, container_path: &str, read_only: bool) {
        self.container_mounts.push(ContainerMount {
            source: guest_path.to_string(),
            destination: container_path.to_string(),
            read_only,
        });
    }

    /// Build container mount configuration.
    pub fn build_container_mounts(&self) -> Vec<ContainerMount> {
        self.container_mounts.clone()
    }
}
