//! File-based lock manager for cross-process locking.
//!
//! This implementation uses flock(2) for actual locking and file existence
//! for allocation tracking. It is multiprocess-safe and suitable for production use.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::{LockId, LockManager, Locker};
use super::{lock_already_allocated, lock_not_allocated, lock_not_found};

/// File-based lock manager for cross-process locking.
///
/// Locks are represented as files in a directory. File existence indicates
/// allocation state, and flock(2) is used for actual locking.
///
/// # Directory Structure
///
/// ```text
/// lock_dir/
/// ├── 0     # Lock file for ID 0
/// ├── 1     # Lock file for ID 1
/// └── ...
/// ```
///
/// # Example
///
/// ```ignore
/// let manager = FileLockManager::new("/var/run/boxlite/locks")?;
/// let lock_id = manager.allocate()?;
/// let lock = manager.retrieve(lock_id)?;
///
/// lock.lock();  // Uses flock(LOCK_EX)
/// // ... critical section ...
/// lock.unlock();  // Uses flock(LOCK_UN)
/// ```
pub struct FileLockManager {
    lock_dir: PathBuf,
    allocated: RwLock<HashSet<LockId>>,
    alloc_lock: Mutex<()>,
}

impl FileLockManager {
    /// Create a new file lock manager at the given directory.
    ///
    /// The directory will be created if it doesn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created.
    pub fn new<P: AsRef<Path>>(lock_dir: P) -> BoxliteResult<Self> {
        let lock_dir = lock_dir.as_ref().to_path_buf();

        // Create directory if it doesn't exist
        fs::create_dir_all(&lock_dir).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to create lock directory {}: {}",
                lock_dir.display(),
                e
            ))
        })?;

        // Scan existing lock files to build allocated set
        let mut allocated = HashSet::new();
        if let Ok(entries) = fs::read_dir(&lock_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str()
                    && let Ok(id) = name.parse::<u32>()
                {
                    allocated.insert(LockId(id));
                }
            }
        }

        Ok(Self {
            lock_dir,
            allocated: RwLock::new(allocated),
            alloc_lock: Mutex::new(()),
        })
    }

    /// Open an existing lock manager directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory doesn't exist.
    pub fn open<P: AsRef<Path>>(lock_dir: P) -> BoxliteResult<Self> {
        let lock_dir = lock_dir.as_ref().to_path_buf();

        if !lock_dir.exists() {
            return Err(BoxliteError::NotFound(format!(
                "lock directory {}",
                lock_dir.display()
            )));
        }

        Self::new(lock_dir)
    }

    /// Get the path to a lock file.
    fn lock_path(&self, id: LockId) -> PathBuf {
        self.lock_dir.join(id.0.to_string())
    }

    /// Find the next available lock ID.
    fn next_available_id(&self) -> LockId {
        let allocated = self.allocated.read().unwrap();
        let mut id = 0u32;
        while allocated.contains(&LockId(id)) {
            id = id.checked_add(1).expect("lock ID overflow");
        }
        LockId(id)
    }
}

impl LockManager for FileLockManager {
    fn allocate(&self) -> BoxliteResult<LockId> {
        let _guard = self.alloc_lock.lock().unwrap();

        // Find next available ID
        let id = self.next_available_id();
        let path = self.lock_path(id);

        // Create lock file with O_EXCL to atomically check and create
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    // Race condition - try again
                    BoxliteError::Internal(format!("lock file already exists: {}", path.display()))
                } else {
                    BoxliteError::Storage(format!(
                        "failed to create lock file {}: {}",
                        path.display(),
                        e
                    ))
                }
            })?;

        drop(file);

        // Track allocation
        self.allocated.write().unwrap().insert(id);

        Ok(id)
    }

    fn retrieve(&self, id: LockId) -> BoxliteResult<Arc<dyn Locker>> {
        let path = self.lock_path(id);

        // Open the lock file
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    lock_not_found(id)
                } else {
                    BoxliteError::Storage(format!(
                        "failed to open lock file {}: {}",
                        path.display(),
                        e
                    ))
                }
            })?;

        Ok(Arc::new(FileLock { id, file }))
    }

    fn allocate_and_retrieve(&self, id: LockId) -> BoxliteResult<Arc<dyn Locker>> {
        let _guard = self.alloc_lock.lock().unwrap();

        // Check if already allocated
        {
            let allocated = self.allocated.read().unwrap();
            if allocated.contains(&id) {
                return Err(lock_already_allocated(id));
            }
        }

        let path = self.lock_path(id);

        // Create lock file with O_EXCL
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    lock_already_allocated(id)
                } else {
                    BoxliteError::Storage(format!(
                        "failed to create lock file {}: {}",
                        path.display(),
                        e
                    ))
                }
            })?;

        // Track allocation
        self.allocated.write().unwrap().insert(id);

        Ok(Arc::new(FileLock { id, file }))
    }

    fn free(&self, id: LockId) -> BoxliteResult<()> {
        let path = self.lock_path(id);

        // Remove from tracking
        {
            let mut allocated = self.allocated.write().unwrap();
            if !allocated.remove(&id) {
                return Err(lock_not_allocated(id));
            }
        }

        // Delete lock file
        fs::remove_file(&path).map_err(|e| {
            // If file doesn't exist, that's okay
            if e.kind() != std::io::ErrorKind::NotFound {
                return BoxliteError::Storage(format!(
                    "failed to remove lock file {}: {}",
                    path.display(),
                    e
                ));
            }
            BoxliteError::Storage(format!("lock file not found: {}", path.display()))
        })?;

        Ok(())
    }

    fn free_all(&self) -> BoxliteResult<()> {
        let mut allocated = self.allocated.write().unwrap();

        // Remove all lock files
        if let Ok(entries) = fs::read_dir(&self.lock_dir) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }

        allocated.clear();
        Ok(())
    }

    fn available(&self) -> BoxliteResult<Option<u32>> {
        // File-based locks have no inherent limit
        Ok(None)
    }

    fn allocated_count(&self) -> BoxliteResult<u32> {
        Ok(self.allocated.read().unwrap().len() as u32)
    }
}

/// A file-based lock using flock(2).
struct FileLock {
    id: LockId,
    file: File,
}

impl Locker for FileLock {
    fn id(&self) -> LockId {
        self.id
    }

    fn lock(&self) {
        let fd = self.file.as_raw_fd();
        let result = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if result != 0 {
            panic!("flock(LOCK_EX) failed: {}", std::io::Error::last_os_error());
        }
    }

    fn unlock(&self) {
        let fd = self.file.as_raw_fd();
        let result = unsafe { libc::flock(fd, libc::LOCK_UN) };
        if result != 0 {
            panic!("flock(LOCK_UN) failed: {}", std::io::Error::last_os_error());
        }
    }

    fn try_lock(&self) -> bool {
        let fd = self.file.as_raw_fd();
        let result = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        result == 0
    }
}

// SAFETY: File handles are thread-safe for flock operations
unsafe impl Send for FileLock {}
unsafe impl Sync for FileLock {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    fn create_test_manager() -> (FileLockManager, TempDir) {
        let temp_dir = TempDir::new().expect("create temp dir");
        let lock_dir = temp_dir.path().join("locks");
        let manager = FileLockManager::new(&lock_dir).expect("create manager");
        (manager, temp_dir)
    }

    #[test]
    fn test_allocate_creates_file() {
        let (manager, _temp) = create_test_manager();

        let id = manager.allocate().unwrap();
        let path = manager.lock_path(id);

        assert!(path.exists(), "lock file should exist");
    }

    #[test]
    fn test_free_removes_file() {
        let (manager, _temp) = create_test_manager();

        let id = manager.allocate().unwrap();
        let path = manager.lock_path(id);

        assert!(path.exists());

        manager.free(id).unwrap();

        assert!(!path.exists(), "lock file should be removed");
    }

    #[test]
    fn test_retrieve_opens_file() {
        let (manager, _temp) = create_test_manager();

        let id = manager.allocate().unwrap();
        let lock = manager.retrieve(id).unwrap();

        assert_eq!(lock.id(), id);
    }

    #[test]
    fn test_lock_unlock() {
        let (manager, _temp) = create_test_manager();

        let id = manager.allocate().unwrap();
        let lock = manager.retrieve(id).unwrap();

        lock.lock();
        lock.unlock();

        // Should be able to lock again
        assert!(lock.try_lock());
        lock.unlock();
    }

    #[test]
    fn test_try_lock_fails_when_held() {
        let (manager, _temp) = create_test_manager();

        let id = manager.allocate().unwrap();
        let lock1 = manager.retrieve(id).unwrap();
        let lock2 = manager.retrieve(id).unwrap();

        lock1.lock();
        assert!(!lock2.try_lock(), "should fail when lock is held");
        lock1.unlock();

        assert!(lock2.try_lock(), "should succeed after unlock");
        lock2.unlock();
    }

    #[test]
    fn test_reopen_manager() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let lock_dir = temp_dir.path().join("locks");

        // Create manager and allocate some locks
        let id1;
        let id2;
        {
            let manager = FileLockManager::new(&lock_dir).unwrap();
            id1 = manager.allocate().unwrap();
            id2 = manager.allocate().unwrap();
        }

        // Reopen manager - should see existing locks
        {
            let manager = FileLockManager::open(&lock_dir).unwrap();
            assert_eq!(manager.allocated_count().unwrap(), 2);

            // Should be able to retrieve
            let lock1 = manager.retrieve(id1).unwrap();
            let lock2 = manager.retrieve(id2).unwrap();
            assert_eq!(lock1.id(), id1);
            assert_eq!(lock2.id(), id2);
        }
    }

    #[test]
    fn test_cross_process_locking() {
        // This test simulates cross-process locking by using threads
        // with separate lock handles (which is how it works with flock)

        let (manager, _temp) = create_test_manager();
        let manager = Arc::new(manager);

        let id = manager.allocate().unwrap();
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let mut handles = vec![];

        for _ in 0..4 {
            let mgr = manager.clone();
            let ctr = counter.clone();
            handles.push(thread::spawn(move || {
                // Each thread gets its own file handle (simulating separate processes)
                let lock = mgr.retrieve(id).unwrap();
                for _ in 0..100 {
                    lock.lock();
                    let val = ctr.load(std::sync::atomic::Ordering::SeqCst);
                    // Small yield to increase chance of race condition if locking is broken
                    thread::yield_now();
                    ctr.store(val + 1, std::sync::atomic::Ordering::SeqCst);
                    lock.unlock();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            counter.load(std::sync::atomic::Ordering::SeqCst),
            400,
            "all increments should complete without races"
        );
    }

    #[test]
    fn test_allocate_and_retrieve() {
        let (manager, _temp) = create_test_manager();

        // Allocate specific ID
        let lock = manager.allocate_and_retrieve(LockId(42)).unwrap();
        assert_eq!(lock.id(), LockId(42));

        // Should fail if already allocated
        assert!(manager.allocate_and_retrieve(LockId(42)).is_err());

        // Normal allocate should skip the used ID
        let id = manager.allocate().unwrap();
        assert_ne!(id, LockId(42));
    }

    #[test]
    fn test_free_all() {
        let (manager, _temp) = create_test_manager();

        let _id1 = manager.allocate().unwrap();
        let _id2 = manager.allocate().unwrap();
        let _id3 = manager.allocate().unwrap();

        assert_eq!(manager.allocated_count().unwrap(), 3);

        manager.free_all().unwrap();

        assert_eq!(manager.allocated_count().unwrap(), 0);
    }
}
