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

    /// Size multiplier numerator (11/10 = 1.1x = 10% overhead)
    pub const SIZE_MULTIPLIER_NUM: u64 = 11;

    /// Size multiplier denominator
    pub const SIZE_MULTIPLIER_DEN: u64 = 10;

    /// Base overhead for ext4 journal (in bytes)
    /// 64MB for journal
    pub const JOURNAL_OVERHEAD_BYTES: u64 = 64 * 1024 * 1024;

    /// Minimum disk size (in bytes)
    /// 256MB for small images
    pub const MIN_DISK_SIZE_BYTES: u64 = 256 * 1024 * 1024;

    /// Default fallback directory size if calculation fails (in bytes)
    pub const DEFAULT_DIR_SIZE_BYTES: u64 = 64 * 1024 * 1024;
}
