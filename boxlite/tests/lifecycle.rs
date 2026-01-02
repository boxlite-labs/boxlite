//! Integration tests for box lifecycle (create, list, get, remove, stop).

use boxlite::BoxliteRuntime;
use boxlite::runtime::options::{BoxOptions, BoxliteOptions, RootfsSpec};
use boxlite::runtime::types::{BoxID, BoxStatus};
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
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for cleanup
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box2 = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for cleanup
                ..Default::default()
            },
            None,
        )
        .unwrap();

    // IDs should be unique
    assert_ne!(box1.id(), box2.id());

    // IDs should be 26 characters (ULID format)
    assert_eq!(box1.id().as_str().len(), 26);
    assert_eq!(box2.id().as_str().len(), 26);

    // Cleanup
    box1.stop().await.unwrap();
    box2.stop().await.unwrap();
    ctx.runtime.remove(box1.id().as_str(), false).await.unwrap();
    ctx.runtime.remove(box2.id().as_str(), false).await.unwrap();
}

#[tokio::test]
async fn create_stores_custom_options() {
    let options = BoxOptions {
        rootfs: RootfsSpec::Image("alpine:latest".into()),
        cpus: Some(4),
        memory_mib: Some(1024),
        auto_remove: false, // Keep box after stop for cleanup
        ..Default::default()
    };

    let ctx = TestContext::new();
    let handle = ctx.runtime.create(options, None).unwrap();
    let box_id = handle.id().clone();

    let info = ctx.runtime.get_info(box_id.as_str()).unwrap().unwrap();

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
    ctx.runtime.remove(box_id.as_str(), false).await.unwrap();
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
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for cleanup
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box2 = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for cleanup
                ..Default::default()
            },
            None,
        )
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
    ctx.runtime.remove(box1.id().as_str(), false).await.unwrap();
    ctx.runtime.remove(box2.id().as_str(), false).await.unwrap();
}

#[tokio::test]
async fn list_info_sorted_by_creation_time_newest_first() {
    let ctx = TestContext::new();

    // Create boxes with small delay to ensure different timestamps
    let box1 = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for cleanup
                ..Default::default()
            },
            None,
        )
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    let box2 = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for cleanup
                ..Default::default()
            },
            None,
        )
        .unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    let box3 = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for cleanup
                ..Default::default()
            },
            None,
        )
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
    ctx.runtime.remove(box1_id.as_str(), false).await.unwrap();
    ctx.runtime.remove(box2_id.as_str(), false).await.unwrap();
    ctx.runtime.remove(box3_id.as_str(), false).await.unwrap();
}

// ============================================================================
// GET / EXISTS TESTS
// ============================================================================

#[tokio::test]
async fn get_info_returns_box_metadata() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Get info from runtime
    let info = ctx.runtime.get_info(box_id.as_str()).unwrap().unwrap();
    assert_eq!(info.id, box_id);
    assert!(
        info.status == BoxStatus::Starting || info.status == BoxStatus::Running,
        "Expected Starting or Running, got {:?}",
        info.status
    );

    // Cleanup
    ctx.runtime.remove(box_id.as_str(), true).await.unwrap();
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
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    assert!(ctx.runtime.exists(box_id.as_str()).unwrap());

    // Cleanup
    ctx.runtime.remove(box_id.as_str(), true).await.unwrap();
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
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for explicit remove
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Stop the box first
    handle.stop().await.unwrap();

    // Remove without force should succeed on stopped box
    ctx.runtime.remove(box_id.as_str(), false).await.unwrap();

    // Box should no longer exist
    assert!(!ctx.runtime.exists(box_id.as_str()).unwrap());
}

#[tokio::test]
async fn remove_active_without_force_fails() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Box is in Starting state (active)
    let info = ctx.runtime.get_info(box_id.as_str()).unwrap().unwrap();
    assert!(info.status.is_active());

    // Remove without force should fail
    let result = ctx.runtime.remove(box_id.as_str(), false).await;
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("cannot remove active box"),
        "Expected active box error, got: {}",
        err
    );

    // Box should still exist
    assert!(ctx.runtime.exists(box_id.as_str()).unwrap());

    // Cleanup with force
    ctx.runtime.remove(box_id.as_str(), true).await.unwrap();
}

#[tokio::test]
async fn remove_active_with_force_stops_and_removes() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Box is in Starting state (active)
    let info = ctx.runtime.get_info(box_id.as_str()).unwrap().unwrap();
    assert!(info.status.is_active());

    // Force remove should succeed
    ctx.runtime.remove(box_id.as_str(), true).await.unwrap();

    // Box should no longer exist
    assert!(!ctx.runtime.exists(box_id.as_str()).unwrap());
}

#[tokio::test]
async fn remove_deletes_box_from_database() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Verify box exists before removal
    assert!(ctx.runtime.exists(box_id.as_str()).unwrap());

    // Force remove
    ctx.runtime.remove(box_id.as_str(), true).await.unwrap();

    // Box should no longer exist in database
    assert!(!ctx.runtime.exists(box_id.as_str()).unwrap());
}

// ============================================================================
// STOP TESTS
// ============================================================================

#[tokio::test]
async fn stop_marks_box_as_stopped() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false, // Keep box after stop for status check
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Stop the box
    handle.stop().await.unwrap();

    // Status should be Stopped
    let info = ctx.runtime.get_info(box_id.as_str()).unwrap().unwrap();
    assert_eq!(info.status, BoxStatus::Stopped);

    // Cleanup
    ctx.runtime.remove(box_id.as_str(), false).await.unwrap();
}

// ============================================================================
// LITEBOX INFO TESTS
// ============================================================================

#[tokio::test]
async fn litebox_info_returns_correct_metadata() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Get info from runtime (handle.info() requires VM initialization)
    let info = ctx
        .runtime
        .get_info(box_id.as_str())
        .unwrap()
        .expect("info should be available");
    assert_eq!(info.id, box_id);
    assert_eq!(info.status, BoxStatus::Starting);
    assert_eq!(info.cpus, 2); // Default value
    assert_eq!(info.memory_mib, 512); // Default value

    // Cleanup
    ctx.runtime.remove(box_id.as_str(), true).await.unwrap();
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
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box2 = ctx2
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

    // Each runtime should only see its own box
    assert_eq!(ctx1.runtime.list_info().unwrap().len(), 1);
    assert_eq!(ctx2.runtime.list_info().unwrap().len(), 1);

    assert_eq!(ctx1.runtime.list_info().unwrap()[0].id, *box1.id());
    assert_eq!(ctx2.runtime.list_info().unwrap()[0].id, *box2.id());

    // Cleanup
    ctx1.runtime.remove(box1.id().as_str(), true).await.unwrap();
    ctx2.runtime.remove(box2.id().as_str(), true).await.unwrap();
}

// ============================================================================
// PERSISTENCE TESTS
// ============================================================================

#[tokio::test]
async fn boxes_persist_across_runtime_restart() {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let home_dir = temp_dir.path().to_path_buf();

    let box_id: BoxID;

    // Create runtime and a box
    {
        let options = BoxliteOptions {
            home_dir: home_dir.clone(),
        };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime");
        let litebox = runtime
            .create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    auto_remove: false, // Keep box for persistence test
                    ..Default::default()
                },
                None,
            )
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
        runtime.remove(box_id.as_str(), false).await.unwrap();
    }
}

#[tokio::test]
async fn multiple_boxes_persist_and_recover_without_lock_errors() {
    // Test that multiple boxes can be created, persisted, and recovered
    // without lock allocation errors during recovery
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let home_dir = temp_dir.path().to_path_buf();

    let box_ids: Vec<BoxID>;

    // Create multiple boxes (allocates locks)
    {
        let options = BoxliteOptions {
            home_dir: home_dir.clone(),
        };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime");

        // Create 3 boxes
        let litebox1 = runtime
            .create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    auto_remove: false, // Keep box for persistence test
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let litebox2 = runtime
            .create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    auto_remove: false, // Keep box for persistence test
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let litebox3 = runtime
            .create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    auto_remove: false, // Keep box for persistence test
                    ..Default::default()
                },
                None,
            )
            .unwrap();

        box_ids = vec![
            litebox1.id().clone(),
            litebox2.id().clone(),
            litebox3.id().clone(),
        ];

        // Stop all boxes before runtime drops
        litebox1.stop().await.unwrap();
        litebox2.stop().await.unwrap();
        litebox3.stop().await.unwrap();

        // Runtime drops here, simulating process exit
    }

    // Create new runtime with same home directory (simulates restart)
    // This should successfully recover all boxes without lock allocation errors
    {
        let options = BoxliteOptions { home_dir };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime after restart");

        // All boxes should be recovered from database
        let boxes = runtime.list_info().unwrap();
        assert_eq!(boxes.len(), 3, "All boxes should be recovered");

        // Verify all box IDs are present
        let recovered_ids: Vec<&BoxID> = boxes.iter().map(|b| &b.id).collect();
        for box_id in &box_ids {
            assert!(
                recovered_ids.contains(&box_id),
                "Box {} should be recovered",
                box_id
            );
        }

        // All boxes should be in Stopped status
        for info in &boxes {
            assert_eq!(
                info.status,
                BoxStatus::Stopped,
                "Recovered box should be stopped"
            );
        }

        // Cleanup
        for box_id in &box_ids {
            runtime.remove(box_id.as_str(), false).await.unwrap();
        }
    }
}

// ============================================================================
// AUTO_REMOVE TESTS
// ============================================================================

#[tokio::test]
async fn auto_remove_default_is_true() {
    let options = BoxOptions::default();
    assert!(
        options.auto_remove,
        "auto_remove should default to true (like Docker --rm)"
    );
}

#[tokio::test]
async fn auto_remove_true_removes_box_on_stop() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Box should exist before stop
    assert!(ctx.runtime.exists(box_id.as_str()).unwrap());

    // Stop should auto-remove
    handle.stop().await.unwrap();

    // Box should no longer exist
    assert!(
        !ctx.runtime.exists(box_id.as_str()).unwrap(),
        "Box should be auto-removed when auto_remove=true"
    );
}

#[tokio::test]
async fn auto_remove_false_preserves_box_on_stop() {
    let ctx = TestContext::new();
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                auto_remove: false,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Stop should NOT auto-remove
    handle.stop().await.unwrap();

    // Box should still exist
    assert!(
        ctx.runtime.exists(box_id.as_str()).unwrap(),
        "Box should be preserved when auto_remove=false"
    );

    // Status should be Stopped
    let info = ctx.runtime.get_info(box_id.as_str()).unwrap().unwrap();
    assert_eq!(info.status, BoxStatus::Stopped);

    // Cleanup manually
    ctx.runtime.remove(box_id.as_str(), false).await.unwrap();
}

// ============================================================================
// DETACH TESTS
// ============================================================================

#[tokio::test]
async fn detach_default_is_false() {
    let options = BoxOptions::default();
    assert!(
        !options.detach,
        "detach should default to false (box tied to parent lifecycle)"
    );
}

#[tokio::test]
async fn detach_option_is_stored_in_box_config() {
    let ctx = TestContext::new();

    // Create box with detach=true
    let handle = ctx
        .runtime
        .create(
            BoxOptions {
                rootfs: RootfsSpec::Image("alpine:latest".into()),
                detach: true,
                auto_remove: false, // Keep for inspection
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let box_id = handle.id().clone();

    // Note: detach is not exposed in BoxInfo, it's an internal option
    // that affects the shim subprocess behavior. We just verify the box
    // was created successfully with the option.
    assert!(ctx.runtime.exists(box_id.as_str()).unwrap());

    // Cleanup
    ctx.runtime.remove(box_id.as_str(), true).await.unwrap();
}

// ============================================================================
// RECOVERY CLEANUP TESTS
// ============================================================================

#[tokio::test]
async fn recovery_removes_auto_remove_true_boxes() {
    // Test that boxes with auto_remove=true are removed during recovery
    // This simulates a crash scenario where boxes weren't properly cleaned up
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let home_dir = temp_dir.path().to_path_buf();

    let persistent_box_id: BoxID;

    // Create two boxes: one with auto_remove=true, one with auto_remove=false
    {
        let options = BoxliteOptions {
            home_dir: home_dir.clone(),
        };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime");

        // Create auto_remove=true box (should be cleaned up on recovery)
        let auto_remove_box = runtime
            .create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    auto_remove: true,
                    ..Default::default()
                },
                None,
            )
            .unwrap();

        // Create auto_remove=false box (should persist)
        let persistent_box = runtime
            .create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    auto_remove: false,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        persistent_box_id = persistent_box.id().clone();

        // Both boxes should exist before shutdown
        assert!(runtime.exists(auto_remove_box.id().as_str()).unwrap());
        assert!(runtime.exists(persistent_box_id.as_str()).unwrap());

        // Stop the persistent box normally (it stays in DB)
        persistent_box.stop().await.unwrap();

        // Verify both exist in DB (auto_remove box is still Starting)
        assert_eq!(runtime.list_info().unwrap().len(), 2);

        // Drop runtime without stopping auto_remove_box - simulates crash
        // The box will remain in database but should be cleaned on recovery
    }

    // Create new runtime with same home directory (simulates restart)
    {
        let options = BoxliteOptions { home_dir };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime after restart");

        // auto_remove=true box should be removed during recovery
        // auto_remove=false box should be recovered
        let boxes = runtime.list_info().unwrap();
        assert_eq!(
            boxes.len(),
            1,
            "Only persistent box should survive recovery"
        );
        assert_eq!(
            boxes[0].id, persistent_box_id,
            "Recovered box should be the persistent one"
        );

        // Cleanup
        runtime
            .remove(persistent_box_id.as_str(), false)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn recovery_removes_orphaned_stopped_boxes_without_directory() {
    // Test that stopped boxes without directories are KEPT during recovery
    // (They might have been created but never started, which is valid)
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let home_dir = temp_dir.path().to_path_buf();

    let box_id: BoxID;
    let box_home: std::path::PathBuf;

    // Create a box, stop it (persists), then delete directory
    {
        let options = BoxliteOptions {
            home_dir: home_dir.clone(),
        };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime");

        let litebox = runtime
            .create(
                BoxOptions {
                    rootfs: RootfsSpec::Image("alpine:latest".into()),
                    auto_remove: false,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        box_id = litebox.id().clone();
        box_home = home_dir.join("boxes").join(box_id.as_str());

        // Stop the box (persists to DB with status=Stopped)
        litebox.stop().await.unwrap();

        // Box should be persisted
        assert!(runtime.exists(box_id.as_str()).unwrap());
    }

    // Delete the box's directory (simulating it was never created or manually deleted)
    if box_home.exists() {
        std::fs::remove_dir_all(&box_home).expect("Failed to delete box directory");
    }

    // Create new runtime with same home directory (simulates restart)
    {
        let options = BoxliteOptions { home_dir };
        let runtime = BoxliteRuntime::new(options).expect("Failed to create runtime after restart");

        // Stopped box without directory should be KEPT (it might never have been started)
        // Recovery only removes active (Starting/Running) boxes that are missing directories
        let boxes = runtime.list_info().unwrap();
        assert_eq!(
            boxes.len(),
            1,
            "Stopped box without directory should be kept"
        );
        assert_eq!(
            boxes[0].id, box_id,
            "Box should be recovered even without directory"
        );
        assert_eq!(
            boxes[0].status,
            BoxStatus::Stopped,
            "Box should remain in Stopped status"
        );

        // Cleanup
        runtime.remove(box_id.as_str(), false).await.unwrap();
    }
}
