//! Integration tests for box lifecycle (create, list, get, remove, stop).

use boxlite::BoxliteRuntime;
use boxlite::runtime::options::{BoxOptions, BoxliteOptions};
use boxlite::runtime::types::BoxStatus;
use boxlite_shared::Transport;
use tempfile::TempDir;

// ============================================================================
// TEST FIXTURES
// ============================================================================

/// Test context with isolated runtime and automatic cleanup.
struct TestContext {
    runtime: BoxliteRuntime,
    _temp_dir: TempDir, // Dropped after test
}

impl TestContext {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let options = BoxliteOptions {
            home_dir: temp_dir.path().to_path_buf(),
        };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime");
        Self {
            runtime,
            _temp_dir: temp_dir,
        }
    }
}

// ============================================================================
// RUNTIME INITIALIZATION TESTS
// ============================================================================

#[test]
fn runtime_initialization_creates_empty_list() {
    let ctx = TestContext::new();
    assert!(ctx.runtime.list_info().unwrap().is_empty());
}

// ============================================================================
// BOX CREATION TESTS
// ============================================================================

#[tokio::test]
async fn create_generates_unique_ulid_ids() {
    let ctx = TestContext::new();
    let box1 = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box2 = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();

    // IDs should be unique
    assert_ne!(box1.id(), box2.id());

    // IDs should be 26 characters (ULID format)
    assert_eq!(box1.id().len(), 26);
    assert_eq!(box2.id().len(), 26);

    // Cleanup
    box1.stop().await.unwrap();
    box2.stop().await.unwrap();
    ctx.runtime.remove(box1.id(), false).await.unwrap();
    ctx.runtime.remove(box2.id(), false).await.unwrap();
}

#[tokio::test]
async fn create_stores_custom_options() {
    let options = BoxOptions {
        cpus: Some(4),
        memory_mib: Some(1024),
        ..Default::default()
    };

    let ctx = TestContext::new();
    let handle = ctx.runtime.create("alpine:latest", options, None).unwrap();
    let box_id = handle.id().clone();

    let info = ctx.runtime.get_info(&box_id).unwrap().unwrap();

    // Verify metadata was stored correctly
    assert_eq!(info.cpus, 4);
    assert_eq!(info.memory_mib, 1024);
    assert!(info.created_at.timestamp() > 0);

    // Verify transport is Unix socket
    match info.transport {
        Transport::Unix { socket_path } => {
            assert!(!socket_path.as_os_str().is_empty());
        }
        _ => panic!("Expected Unix transport"),
    }

    // Cleanup
    handle.stop().await.unwrap();
    ctx.runtime.remove(&box_id, false).await.unwrap();
}

// ============================================================================
// LIST TESTS
// ============================================================================

#[tokio::test]
async fn list_info_returns_all_boxes() {
    let ctx = TestContext::new();

    // Initially empty
    assert_eq!(ctx.runtime.list_info().unwrap().len(), 0);

    // Create two boxes
    let box1 = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box2 = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();

    // List should show both boxes
    let boxes = ctx.runtime.list_info().unwrap();
    assert_eq!(boxes.len(), 2);

    let ids: Vec<&str> = boxes.iter().map(|b| b.id.as_str()).collect();
    assert!(ids.contains(&box1.id().as_str()));
    assert!(ids.contains(&box2.id().as_str()));

    // Cleanup
    box1.stop().await.unwrap();
    box2.stop().await.unwrap();
    ctx.runtime.remove(box1.id(), false).await.unwrap();
    ctx.runtime.remove(box2.id(), false).await.unwrap();
}

#[tokio::test]
async fn list_info_sorted_by_creation_time_newest_first() {
    let ctx = TestContext::new();

    // Create boxes with small delay to ensure different timestamps
    let box1 = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    let box2 = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    let box3 = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();

    // List should be sorted newest first
    let boxes = ctx.runtime.list_info().unwrap();
    assert_eq!(boxes.len(), 3);
    assert_eq!(boxes[0].id, *box3.id()); // Newest
    assert_eq!(boxes[1].id, *box2.id());
    assert_eq!(boxes[2].id, *box1.id()); // Oldest

    // Cleanup - must stop handles before remove since they have is_shutdown flag
    let box1_id = box1.id().clone();
    let box2_id = box2.id().clone();
    let box3_id = box3.id().clone();
    box1.stop().await.unwrap();
    box2.stop().await.unwrap();
    box3.stop().await.unwrap();
    ctx.runtime.remove(&box1_id, false).await.unwrap();
    ctx.runtime.remove(&box2_id, false).await.unwrap();
    ctx.runtime.remove(&box3_id, false).await.unwrap();
}

// ============================================================================
// GET / EXISTS TESTS
// ============================================================================

#[tokio::test]
async fn get_info_returns_box_metadata() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    // Get info from runtime
    let info = ctx.runtime.get_info(&box_id).unwrap().unwrap();
    assert_eq!(info.id, box_id);
    assert!(
        info.status == BoxStatus::Starting || info.status == BoxStatus::Running,
        "Expected Starting or Running, got {:?}",
        info.status
    );

    // Cleanup
    ctx.runtime.remove(&box_id, true).await.unwrap();
}

#[tokio::test]
async fn get_info_returns_none_for_nonexistent() {
    let ctx = TestContext::new();
    let missing = ctx.runtime.get_info("nonexistent-id").unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn exists_returns_true_for_existing_box() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    assert!(ctx.runtime.exists(&box_id).unwrap());

    // Cleanup
    ctx.runtime.remove(&box_id, true).await.unwrap();
}

#[tokio::test]
async fn exists_returns_false_for_nonexistent() {
    let ctx = TestContext::new();
    assert!(!ctx.runtime.exists("nonexistent-id").unwrap());
}

// ============================================================================
// REMOVE TESTS (BoxliteRuntime::remove)
// ============================================================================

#[tokio::test]
async fn remove_nonexistent_returns_not_found() {
    let ctx = TestContext::new();
    let result = ctx.runtime.remove("nonexistent-id", false).await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "Expected NotFound error, got: {}",
        err
    );
}

#[tokio::test]
async fn remove_stopped_box_succeeds() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    // Stop the box first
    handle.stop().await.unwrap();

    // Remove without force should succeed on stopped box
    ctx.runtime.remove(&box_id, false).await.unwrap();

    // Box should no longer exist
    assert!(!ctx.runtime.exists(&box_id).unwrap());
}

#[tokio::test]
async fn remove_active_without_force_fails() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    // Box is in Starting state (active)
    let info = ctx.runtime.get_info(&box_id).unwrap().unwrap();
    assert!(info.status.is_active());

    // Remove without force should fail
    let result = ctx.runtime.remove(&box_id, false).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("cannot remove active box"),
        "Expected active box error, got: {}",
        err
    );

    // Box should still exist
    assert!(ctx.runtime.exists(&box_id).unwrap());

    // Cleanup with force
    ctx.runtime.remove(&box_id, true).await.unwrap();
}

#[tokio::test]
async fn remove_active_with_force_stops_and_removes() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    // Box is in Starting state (active)
    let info = ctx.runtime.get_info(&box_id).unwrap().unwrap();
    assert!(info.status.is_active());

    // Force remove should succeed
    ctx.runtime.remove(&box_id, true).await.unwrap();

    // Box should no longer exist
    assert!(!ctx.runtime.exists(&box_id).unwrap());
}

#[tokio::test]
async fn remove_deletes_box_from_database() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    // Verify box exists before removal
    assert!(ctx.runtime.exists(&box_id).unwrap());

    // Force remove
    ctx.runtime.remove(&box_id, true).await.unwrap();

    // Box should no longer exist in database
    assert!(!ctx.runtime.exists(&box_id).unwrap());
}

// ============================================================================
// STOP TESTS
// ============================================================================

#[tokio::test]
async fn stop_marks_box_as_stopped() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    // Stop the box
    handle.stop().await.unwrap();

    // Status should be Stopped
    let info = ctx.runtime.get_info(&box_id).unwrap().unwrap();
    assert_eq!(info.status, BoxStatus::Stopped);

    // Cleanup
    ctx.runtime.remove(&box_id, false).await.unwrap();
}

// ============================================================================
// LITEBOX INFO TESTS
// ============================================================================

#[tokio::test]
async fn litebox_info_returns_correct_metadata() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box_id = handle.id().clone();

    // Get info from runtime (handle.info() requires VM initialization)
    let info = ctx
        .runtime
        .get_info(&box_id)
        .unwrap()
        .expect("info should be available");
    assert_eq!(info.id, box_id);
    assert_eq!(info.status, BoxStatus::Starting);
    assert_eq!(info.cpus, 2); // Default value
    assert_eq!(info.memory_mib, 512); // Default value

    // Cleanup
    ctx.runtime.remove(&box_id, true).await.unwrap();
}

// ============================================================================
// ISOLATION TESTS
// ============================================================================

#[tokio::test]
async fn multiple_runtimes_are_isolated() {
    let ctx1 = TestContext::new();
    let ctx2 = TestContext::new();

    let box1 = ctx1
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();
    let box2 = ctx2
        .runtime
        .create("alpine:latest", BoxOptions::default(), None)
        .unwrap();

    // Each runtime should only see its own box
    assert_eq!(ctx1.runtime.list_info().unwrap().len(), 1);
    assert_eq!(ctx2.runtime.list_info().unwrap().len(), 1);

    assert_eq!(ctx1.runtime.list_info().unwrap()[0].id, *box1.id());
    assert_eq!(ctx2.runtime.list_info().unwrap()[0].id, *box2.id());

    // Cleanup
    ctx1.runtime.remove(box1.id(), true).await.unwrap();
    ctx2.runtime.remove(box2.id(), true).await.unwrap();
}

// ============================================================================
// PERSISTENCE TESTS
// ============================================================================

#[tokio::test]
async fn boxes_persist_across_runtime_restart() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let home_dir = temp_dir.path().to_path_buf();

    let box_id: String;

    // Create runtime and a box
    {
        let options = BoxliteOptions {
            home_dir: home_dir.clone(),
        };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime");
        let litebox = runtime
            .create("alpine:latest", BoxOptions::default(), None)
            .unwrap();
        box_id = litebox.id().clone();

        // Box should be in database
        let boxes = runtime.list_info().unwrap();
        assert_eq!(boxes.len(), 1);

        // Stop the box before "restart"
        litebox.stop().await.unwrap();
    }

    // Create new runtime with same home directory (simulates restart)
    {
        let options = BoxliteOptions { home_dir };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime");

        // Box should be recovered from database
        let boxes = runtime.list_info().unwrap();
        assert_eq!(boxes.len(), 1);

        // Status should be Stopped
        let status = &boxes[0].status;
        assert_eq!(status, &BoxStatus::Stopped);

        // Cleanup
        runtime.remove(&box_id, false).await.unwrap();
    }
}
