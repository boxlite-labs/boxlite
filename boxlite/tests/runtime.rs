//! Integration tests for runtime initialization and locking behavior.

use boxlite::BoxliteRuntime;
use boxlite::runtime::options::BoxliteOptions;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn test_runtime_prevents_concurrent_access() {
    let temp_dir = TempDir::new().unwrap();

    // Create first runtime
    let config1 = BoxliteOptions {
        home_dir: temp_dir.path().to_path_buf(),
    };
    let runtime1 = BoxliteRuntime::new(config1).unwrap();

    // Try to create second runtime (should fail)
    let config2 = BoxliteOptions {
        home_dir: temp_dir.path().to_path_buf(),
    };
    let result = BoxliteRuntime::new(config2);
    assert!(result.is_err());

    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("Another BoxliteRuntime"));
    assert!(err_msg.contains("already using directory"));

    // Drop first runtime
    drop(runtime1);

    // Now should be able to create another
    let config3 = BoxliteOptions {
        home_dir: temp_dir.path().to_path_buf(),
    };
    let _runtime2 = BoxliteRuntime::new(config3).unwrap();
}

#[test]
fn test_runtime_lock_released_on_drop() {
    let temp_dir = TempDir::new().unwrap();

    // Create and drop runtime
    {
        let config = BoxliteOptions {
            home_dir: temp_dir.path().to_path_buf(),
        };
        let _runtime = BoxliteRuntime::new(config).unwrap();
    } // Lock released here

    // Should be able to create new runtime
    let config2 = BoxliteOptions {
        home_dir: temp_dir.path().to_path_buf(),
    };
    let _runtime2 = BoxliteRuntime::new(config2).unwrap();
}

#[test]
fn test_runtime_lock_across_threads() {
    let temp_dir = TempDir::new().unwrap();
    let dir_path = temp_dir.path().to_path_buf();

    // Acquire lock in main thread
    let config1 = BoxliteOptions {
        home_dir: dir_path.clone(),
    };
    let _runtime1 = BoxliteRuntime::new(config1).unwrap();

    // Try to acquire in another thread (should fail)
    let dir_clone = dir_path.clone();
    let handle = thread::spawn(move || {
        let config = BoxliteOptions {
            home_dir: dir_clone,
        };
        BoxliteRuntime::new(config)
    });

    let result = handle.join().unwrap();
    assert!(result.is_err());
}

#[test]
fn test_different_home_dirs_independent() {
    let temp_dir1 = TempDir::new().unwrap();
    let temp_dir2 = TempDir::new().unwrap();

    // Create runtime in first directory
    let config1 = BoxliteOptions {
        home_dir: temp_dir1.path().to_path_buf(),
    };
    let _runtime1 = BoxliteRuntime::new(config1).unwrap();

    // Should be able to create runtime in second directory
    let config2 = BoxliteOptions {
        home_dir: temp_dir2.path().to_path_buf(),
    };
    let _runtime2 = BoxliteRuntime::new(config2).unwrap();

    // Both should coexist
    drop(_runtime1);
    drop(_runtime2);
}

#[test]
fn test_lock_file_created() {
    let temp_dir = TempDir::new().unwrap();

    let config = BoxliteOptions {
        home_dir: temp_dir.path().to_path_buf(),
    };
    let _runtime = BoxliteRuntime::new(config).unwrap();

    // Lock file should exist
    let lock_file = temp_dir.path().join(".lock");
    assert!(lock_file.exists());
}

#[test]
fn test_lock_survives_short_operations() {
    let temp_dir = TempDir::new().unwrap();

    let config1 = BoxliteOptions {
        home_dir: temp_dir.path().to_path_buf(),
    };
    let runtime = BoxliteRuntime::new(config1).unwrap();

    // Do some operations
    thread::sleep(Duration::from_millis(100));

    // Lock should still be held
    let config2 = BoxliteOptions {
        home_dir: temp_dir.path().to_path_buf(),
    };
    let result = BoxliteRuntime::new(config2);
    assert!(result.is_err());

    drop(runtime);
}
