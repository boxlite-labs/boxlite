//! In-memory lock manager for testing.
//!
//! This implementation uses atomic spinlocks and is NOT multiprocess-safe.
//! It should only be used for unit and integration testing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use boxlite_shared::errors::BoxliteResult;

use super::{LockId, LockManager, Locker};
use super::{lock_already_allocated, lock_exhausted, lock_invalid, lock_not_allocated};

/// In-memory lock manager for testing.
///
/// This manager pre-allocates a fixed number of locks and uses atomic spinlocks.
/// It is NOT multiprocess-safe and should only be used for testing.
///
/// # Example
///
/// ```
/// use boxlite::lock::InMemoryLockManager;
///
/// let manager = InMemoryLockManager::new(16);
/// let lock_id = manager.allocate().expect("allocate lock");
/// let lock = manager.retrieve(lock_id).expect("retrieve lock");
///
/// lock.lock();
/// // ... critical section ...
/// lock.unlock();
/// ```
pub struct InMemoryLockManager {
    locks: Vec<Arc<InMemoryLock>>,
    num_locks: u32,
    alloc_lock: Mutex<()>,
}

struct InMemoryLock {
    id: LockId,
    locked: AtomicBool,
    allocated: AtomicBool,
}

impl InMemoryLockManager {
    /// Create a new in-memory lock manager with the given number of locks.
    ///
    /// # Panics
    ///
    /// Panics if `num_locks` is 0.
    pub fn new(num_locks: u32) -> Self {
        assert!(num_locks > 0, "must provide a non-zero number of locks");

        let locks: Vec<_> = (0..num_locks)
            .map(|i| {
                Arc::new(InMemoryLock {
                    id: LockId(i),
                    locked: AtomicBool::new(false),
                    allocated: AtomicBool::new(false),
                })
            })
            .collect();

        Self {
            locks,
            num_locks,
            alloc_lock: Mutex::new(()),
        }
    }
}

impl LockManager for InMemoryLockManager {
    fn allocate(&self) -> BoxliteResult<LockId> {
        let _guard = self.alloc_lock.lock().unwrap();

        for lock in &self.locks {
            if !lock.allocated.load(Ordering::SeqCst) {
                lock.allocated.store(true, Ordering::SeqCst);
                return Ok(lock.id);
            }
        }

        Err(lock_exhausted())
    }

    fn retrieve(&self, id: LockId) -> BoxliteResult<Arc<dyn Locker>> {
        if id.0 >= self.num_locks {
            return Err(lock_invalid(id, self.num_locks));
        }

        Ok(Arc::new(InMemoryLocker {
            lock: self.locks[id.0 as usize].clone(),
        }))
    }

    fn allocate_and_retrieve(&self, id: LockId) -> BoxliteResult<Arc<dyn Locker>> {
        if id.0 >= self.num_locks {
            return Err(lock_invalid(id, self.num_locks));
        }

        let lock = &self.locks[id.0 as usize];
        if lock.allocated.swap(true, Ordering::SeqCst) {
            return Err(lock_already_allocated(id));
        }

        Ok(Arc::new(InMemoryLocker { lock: lock.clone() }))
    }

    fn free(&self, id: LockId) -> BoxliteResult<()> {
        if id.0 >= self.num_locks {
            return Err(lock_invalid(id, self.num_locks));
        }

        let lock = &self.locks[id.0 as usize];
        if !lock.allocated.swap(false, Ordering::SeqCst) {
            return Err(lock_not_allocated(id));
        }

        Ok(())
    }

    fn free_all(&self) -> BoxliteResult<()> {
        for lock in &self.locks {
            lock.allocated.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    fn available(&self) -> BoxliteResult<Option<u32>> {
        let count = self
            .locks
            .iter()
            .filter(|l| !l.allocated.load(Ordering::SeqCst))
            .count() as u32;
        Ok(Some(count))
    }

    fn allocated_count(&self) -> BoxliteResult<u32> {
        let count = self
            .locks
            .iter()
            .filter(|l| l.allocated.load(Ordering::SeqCst))
            .count() as u32;
        Ok(count)
    }

    fn clear_all_locks(&self) -> BoxliteResult<()> {
        for lock in &self.locks {
            lock.allocated.store(false, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Handle to an in-memory lock.
struct InMemoryLocker {
    lock: Arc<InMemoryLock>,
}

impl Locker for InMemoryLocker {
    fn id(&self) -> LockId {
        self.lock.id
    }

    fn lock(&self) {
        // Spin until we acquire the lock
        while self
            .lock
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::thread::yield_now();
        }
    }

    fn unlock(&self) {
        self.lock.locked.store(false, Ordering::Release);
    }

    fn try_lock(&self) -> bool {
        self.lock
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_allocate_and_free() {
        let manager = InMemoryLockManager::new(4);

        // Allocate all locks
        let id1 = manager.allocate().unwrap();
        let id2 = manager.allocate().unwrap();
        let id3 = manager.allocate().unwrap();
        let id4 = manager.allocate().unwrap();

        // Should be exhausted
        assert!(manager.allocate().is_err());

        // Free one
        manager.free(id2).unwrap();

        // Should be able to allocate again
        let id5 = manager.allocate().unwrap();
        assert_eq!(id5, id2); // Should reuse the freed ID

        // Clean up
        manager.free(id1).unwrap();
        manager.free(id3).unwrap();
        manager.free(id4).unwrap();
        manager.free(id5).unwrap();
    }

    #[test]
    fn test_retrieve() {
        let manager = InMemoryLockManager::new(4);
        let id = manager.allocate().unwrap();

        let lock1 = manager.retrieve(id).unwrap();
        let lock2 = manager.retrieve(id).unwrap();

        assert_eq!(lock1.id(), id);
        assert_eq!(lock2.id(), id);
    }

    #[test]
    fn test_lock_unlock() {
        let manager = InMemoryLockManager::new(4);
        let id = manager.allocate().unwrap();
        let lock = manager.retrieve(id).unwrap();

        lock.lock();
        lock.unlock();

        // Should be able to lock again
        assert!(lock.try_lock());
        lock.unlock();
    }

    #[test]
    fn test_try_lock_contention() {
        let manager = Arc::new(InMemoryLockManager::new(4));
        let id = manager.allocate().unwrap();
        let lock = manager.retrieve(id).unwrap();

        // Lock it
        lock.lock();

        // Try from another "retrieve" - should fail
        let lock2 = manager.retrieve(id).unwrap();
        assert!(!lock2.try_lock());

        // Unlock and try again
        lock.unlock();
        assert!(lock2.try_lock());
        lock2.unlock();
    }

    #[test]
    fn test_concurrent_locking() {
        let manager = Arc::new(InMemoryLockManager::new(4));
        let id = manager.allocate().unwrap();

        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let mut handles = vec![];

        for _ in 0..4 {
            let mgr = manager.clone();
            let ctr = counter.clone();
            handles.push(thread::spawn(move || {
                let lock = mgr.retrieve(id).unwrap();
                for _ in 0..100 {
                    lock.lock();
                    // Increment counter (should be safe under lock)
                    let val = ctr.load(Ordering::SeqCst);
                    ctr.store(val + 1, Ordering::SeqCst);
                    lock.unlock();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        // All increments should have happened
        assert_eq!(counter.load(Ordering::SeqCst), 400);
    }

    #[test]
    fn test_available_count() {
        let manager = InMemoryLockManager::new(4);

        assert_eq!(manager.available().unwrap(), Some(4));
        assert_eq!(manager.allocated_count().unwrap(), 0);

        let id1 = manager.allocate().unwrap();
        let _id2 = manager.allocate().unwrap();

        assert_eq!(manager.available().unwrap(), Some(2));
        assert_eq!(manager.allocated_count().unwrap(), 2);

        manager.free(id1).unwrap();

        assert_eq!(manager.available().unwrap(), Some(3));
        assert_eq!(manager.allocated_count().unwrap(), 1);
    }
}
