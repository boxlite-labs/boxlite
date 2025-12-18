//! Shared constants between host and guest
//!
//! These constants must be identical on both sides of the host-guest boundary.

/// Container runtime constants
pub mod container {
    /// Default container hostname
    pub const DEFAULT_HOSTNAME: &str = "boxlite";

    /// Default RLIMIT_NOFILE soft limit
    pub const RLIMIT_NOFILE_SOFT: u64 = 1024;

    /// Default RLIMIT_NOFILE hard limit
    pub const RLIMIT_NOFILE_HARD: u64 = 1024;
}

/// Network constants
pub mod network {
    /// Default vsock port for guest agent gRPC server
    /// Port 2695 = "BOXL" on phone keypad
    pub const GUEST_AGENT_PORT: u32 = 2695;

    /// Vsock port for guest ready notification
    /// Guest connects to this port to signal it's ready to serve
    /// Port 2696 = "BOXM" on phone keypad
    pub const GUEST_READY_PORT: u32 = 2696;
}

/// Executor environment variable
///
/// Used to specify which executor to use for command execution.
pub mod executor {
    /// Environment variable name for executor selection
    pub const ENV_VAR: &str = "BOXLITE_EXECUTOR";

    /// Value for guest executor (run directly on guest)
    pub const GUEST: &str = "guest";

    /// Key for container executor (format: "container")
    pub const CONTAINER_KEY: &str = "container";
}

/// Virtiofs mount tags
///
/// These tags identify shared filesystems mounted via virtiofs.
/// They must match between host (when creating shares) and guest (when mounting).
pub mod mount_tags {
    /// Tag for prepared rootfs virtiofs share (merged mode)
    pub const ROOTFS: &str = "rootfs";

    /// Tag for extracted layers directory (overlayfs mode)
    pub const LAYERS: &str = "layers";

    /// Tag for shared container directory (contains overlayfs/ and rootfs/)
    pub const SHARED: &str = "shared";
}
