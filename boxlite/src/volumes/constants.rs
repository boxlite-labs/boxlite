//! Storage and disk image constants.
//!
//! Centralized location for all storage-related configuration values.

/// QCOW2 disk image configuration
pub mod qcow2 {
    /// Default disk size in GB (sparse, grows as needed)
    pub const DEFAULT_DISK_SIZE_GB: u64 = 10;

    /// QCOW2 cluster size in bits (64KB = 2^16)
    pub const CLUSTER_BITS: usize = 16;

    /// QCOW2 refcount order (16-bit refcounts = 2^4)
    pub const REFCOUNT_ORDER: u8 = 4;

    /// Block size for QCOW2 formatting (512 bytes)
    pub const BLOCK_SIZE: usize = 512;
}

/// Ext4 filesystem configuration
pub mod ext4 {
    /// Ext4 block size in bytes
    pub const BLOCK_SIZE: u64 = 4096;

    /// Ext4 inode size in bytes
    pub const INODE_SIZE: u64 = 256;

    /// Multiplier for directory size when calculating disk size
    /// (accounts for filesystem overhead)
    pub const SIZE_MULTIPLIER: u64 = 2;

    /// Base overhead for ext4 metadata and journal (in bytes)
    /// Typically 256MB for journal and metadata
    pub const METADATA_OVERHEAD_BYTES: u64 = 256 * 1024 * 1024;

    /// Minimum disk size (in bytes) to handle images with many files
    /// Set to 1GB to accommodate large binaries
    pub const MIN_DISK_SIZE_BYTES: u64 = 1024 * 1024 * 1024;

    /// Default fallback directory size if calculation fails (in bytes)
    pub const DEFAULT_DIR_SIZE_BYTES: u64 = 64 * 1024 * 1024;
}
