//! Lock management for per-entity multiprocess-safe locking.
//!
//! This module provides a lock manager that allocates unique locks for entities
//! (boxes, volumes, etc.). Locks are identified by a numeric ID and can be
//! retrieved by that ID across process restarts.
//!
//! Two implementations are provided:
//! - [`InMemoryLockManager`]: Single-process locks for testing
//! - [`FileLockManager`]: Cross-process locks using flock(2)

mod file;
mod memory;

pub use file::FileLockManager;
pub use memory::InMemoryLockManager;

use std::sync::Arc;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

/// Unique identifier for a lock.
///
/// Lock IDs are assigned by the [`LockManager`] and persisted alongside
/// entity configuration. The same ID always refers to the same underlying lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LockId(pub u32);

impl std::fmt::Display for LockId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Manager for allocating and retrieving multiprocess-safe locks.
///
/// Locks returned by a `LockManager` must be multiprocess-safe: allocating a lock
/// in process A and retrieving that lock's ID in process B must return handles
/// for the same underlying lock.
///
/// # Lock Lifecycle
///
/// 1. Call [`allocate`](LockManager::allocate) to get a new lock
/// 2. Store the [`LockId`] in persistent storage (e.g., database)
/// 3. Use [`retrieve`](LockManager::retrieve) to get the lock handle when needed
/// 4. Call [`free`](LockManager::free) when the entity is removed
///
/// # Example
///
/// ```ignore
/// let manager = FileLockManager::new("/var/run/boxlite/locks", 1024)?;
///
/// // Allocate a lock for a new box
/// let lock_id = manager.allocate()?;
/// box_config.lock_id = lock_id;
/// save_to_database(&box_config);
///
/// // Later, retrieve the lock
/// let lock = manager.retrieve(lock_id)?;
/// lock.lock();
/// // ... critical section ...
/// lock.unlock();
///
/// // When box is removed
/// manager.free(lock_id)?;
/// ```
pub trait LockManager: Send + Sync {
    /// Allocate a new lock and return its unique ID.
    ///
    /// The returned lock is guaranteed not to be returned again by `allocate`
    /// until [`free`](LockManager::free) is called on it.
    ///
    /// # Errors
    ///
    /// Returns an error if all available locks have been allocated.
    fn allocate(&self) -> BoxliteResult<LockId>;

    /// Retrieve a lock by its ID.
    ///
    /// The returned lock handle refers to the same underlying lock as any other
    /// handle retrieved with the same ID, even across different processes.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock ID is invalid.
    fn retrieve(&self, id: LockId) -> BoxliteResult<Arc<dyn Locker>>;

    /// Mark a lock with the given ID as allocated and return it.
    ///
    /// This is used after a restart to reclaim locks that were previously
    /// allocated but whose allocation state was lost.
    ///
    /// # Errors
    ///
    /// Returns an error if the lock is already allocated or the ID is invalid.
    fn allocate_and_retrieve(&self, id: LockId) -> BoxliteResult<Arc<dyn Locker>>;

    /// Free a lock, allowing it to be reallocated.
    ///
    /// After calling this, the lock ID may be returned by a future call
    /// to [`allocate`](LockManager::allocate).
    ///
    /// # Errors
    ///
    /// Returns an error if the lock is not currently allocated.
    fn free(&self, id: LockId) -> BoxliteResult<()>;

    /// Free all locks.
    ///
    /// # Safety
    ///
    /// This is dangerous and should only be used during testing or lock
    /// renumbering operations. If other processes hold locks, this may
    /// cause inconsistent state.
    fn free_all(&self) -> BoxliteResult<()>;

    /// Get the number of available (unallocated) locks.
    ///
    /// Returns `None` if the implementation has no maximum limit.
    fn available(&self) -> BoxliteResult<Option<u32>>;

    /// Get the number of currently allocated locks.
    fn allocated_count(&self) -> BoxliteResult<u32>;
}

/// A lock that provides mutual exclusion.
///
/// All locks with the same ID refer to the same underlying lock, even across
/// different processes. Locking in one process will block other processes
/// attempting to acquire the same lock.
pub trait Locker: Send + Sync {
    /// Get the lock's unique ID.
    fn id(&self) -> LockId;

    /// Acquire the lock, blocking until it becomes available.
    ///
    /// # Panics
    ///
    /// May panic if the lock cannot be acquired due to a fatal error.
    fn lock(&self);

    /// Release the lock.
    ///
    /// # Panics
    ///
    /// May panic if the lock is not held or cannot be released.
    fn unlock(&self);

    /// Try to acquire the lock without blocking.
    ///
    /// Returns `true` if the lock was acquired, `false` if it was already held.
    fn try_lock(&self) -> bool;
}

/// Convenience guard for RAII-style lock management.
pub struct LockGuard<'a> {
    lock: &'a dyn Locker,
}

impl<'a> LockGuard<'a> {
    /// Create a new guard, acquiring the lock.
    pub fn new(lock: &'a dyn Locker) -> Self {
        lock.lock();
        Self { lock }
    }

    /// Try to create a new guard without blocking.
    ///
    /// Returns `None` if the lock is already held.
    pub fn try_new(lock: &'a dyn Locker) -> Option<Self> {
        if lock.try_lock() {
            Some(Self { lock })
        } else {
            None
        }
    }
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

// Error helpers
pub(crate) fn lock_exhausted() -> BoxliteError {
    BoxliteError::Internal("all locks have been allocated".to_string())
}

pub(crate) fn lock_not_found(id: LockId) -> BoxliteError {
    BoxliteError::NotFound(format!("lock {}", id))
}

pub(crate) fn lock_already_allocated(id: LockId) -> BoxliteError {
    BoxliteError::InvalidState(format!("lock {} is already allocated", id))
}

pub(crate) fn lock_not_allocated(id: LockId) -> BoxliteError {
    BoxliteError::InvalidState(format!("lock {} is not allocated", id))
}

pub(crate) fn lock_invalid(id: LockId, max: u32) -> BoxliteError {
    BoxliteError::InvalidArgument(format!("lock ID {} is too large (max: {})", id, max - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock_manager(manager: &dyn LockManager) {
        // Allocate a lock
        let id1 = manager.allocate().expect("allocate first lock");
        let id2 = manager.allocate().expect("allocate second lock");
        assert_ne!(id1, id2, "lock IDs should be unique");

        // Retrieve locks
        let lock1 = manager.retrieve(id1).expect("retrieve first lock");
        let lock2 = manager.retrieve(id2).expect("retrieve second lock");
        assert_eq!(lock1.id(), id1);
        assert_eq!(lock2.id(), id2);

        // Lock and unlock
        lock1.lock();
        lock1.unlock();

        // Try lock
        assert!(lock2.try_lock(), "try_lock should succeed");
        lock2.unlock();

        // Free locks
        manager.free(id1).expect("free first lock");
        manager.free(id2).expect("free second lock");

        // Allocate again - should get the freed IDs back
        let id3 = manager.allocate().expect("allocate after free");
        assert!(id3 == id1 || id3 == id2, "should reuse freed lock");
    }

    #[test]
    fn test_in_memory_manager() {
        let manager = InMemoryLockManager::new(16);
        test_lock_manager(&manager);
    }

    #[test]
    fn test_file_manager() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let lock_path = temp_dir.path().join("locks");
        let manager = FileLockManager::new(&lock_path).expect("create file lock manager");
        test_lock_manager(&manager);
    }

    #[test]
    fn test_lock_guard() {
        let manager = InMemoryLockManager::new(16);
        let id = manager.allocate().expect("allocate");
        let lock = manager.retrieve(id).expect("retrieve");

        {
            let _guard = LockGuard::new(lock.as_ref());
            // Lock is held here
            assert!(!lock.try_lock(), "should not be able to acquire held lock");
        }
        // Lock is released here

        assert!(lock.try_lock(), "should be able to acquire released lock");
        lock.unlock();
    }
}
