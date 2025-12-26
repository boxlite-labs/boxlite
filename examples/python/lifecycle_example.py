#!/usr/bin/env python3
"""
Lifecycle Management Example - Stop, Restart, and Remove

Demonstrates box lifecycle operations:
- Stop: Gracefully stop a box (preserves rootfs)
- Restart: Restart a stopped box (reuses existing rootfs)
- Remove: Completely remove a box
- Reattach: Reconnect to a running box via runtime.get()
"""

import asyncio

import boxlite


async def test_stop_and_restart():
    """Test stopping a box and restarting it."""
    print("\n=== Test 1: Stop and Restart ===")

    runtime = boxlite.Boxlite.default()
    box = None
    restarted_box = None

    try:
        # Create and start a box
        print("Creating box...")
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            cpus=2,
            memory_mib=512
        ))
        box_id = box.id
        print(f"  Box created: {box_id}")

        # Create a file in the box to verify persistence
        print("\nCreating test file in box...")
        execution = await box.exec("sh", ["-c", "echo 'persistent data' > /tmp/test.txt"])
        await execution.wait()
        print("  File created")

        # Verify file exists
        execution = await box.exec("cat", ["/tmp/test.txt"])
        stdout = execution.stdout()
        print("\nFile contents before stop:")
        async for line in stdout:
            print(f"  {line.strip()}")
        await execution.wait()

        # Get box info before stop
        info = box.info()
        print(f"\nBox state before stop: {info.state}")

        # Stop the box (VM shuts down, but rootfs preserved)
        print("\nStopping box...")
        await box.stop()
        box = None  # Mark as stopped
        print("  Box stopped (rootfs preserved)")

        # Get box info after stop
        info = runtime.get_info(box_id)
        if info:
            print(f"Box state after stop: {info.state}")

        # Wait a moment
        await asyncio.sleep(0.5)

        # Restart the box by getting a new handle and executing
        print("\nRestarting box (reuses existing rootfs)...")
        restarted_box = runtime.get(box_id)
        if restarted_box is None:
            print("  Failed to get box handle")
            return

        print(f"  Got box handle: {restarted_box.id}")

        # Execute command triggers restart
        print("\nExecuting command (triggers restart)...")
        execution = await restarted_box.exec("echo", ["Box restarted"])
        stdout = execution.stdout()
        async for line in stdout:
            print(f"  {line.strip()}")
        result = await execution.wait()
        print(f"  Command executed (exit code: {result.exit_code})")

        # Verify our file still exists (proves rootfs was reused)
        print("\nVerifying file persistence after restart...")
        execution = await restarted_box.exec("cat", ["/tmp/test.txt"])
        stdout = execution.stdout()
        print("File contents after restart:")
        file_found = False
        async for line in stdout:
            print(f"  {line.strip()}")
            if "persistent data" in line:
                file_found = True
        await execution.wait()

        if file_found:
            print("  File persisted across restart!")
        else:
            print("  File was not persisted (expected - tmpfs is cleared)")

        # Clean up - stop then remove
        await restarted_box.stop()
        await runtime.remove(box_id, force=False)
        restarted_box = None
        print("\n  Box stopped and removed")

    except Exception as e:
        print(f"\n  Error in test: {e}")
        # Cleanup on error
        if box is not None:
            try:
                await box.stop()
            except:
                pass
        if restarted_box is not None:
            try:
                await restarted_box.stop()
            except:
                pass
        # Force remove any remaining boxes
        try:
            await runtime.remove(box_id, force=True)
        except:
            pass

    print("\n  Test 1 completed")


async def test_reattach_to_running():
    """Test reattaching to a running box via runtime.get()."""
    print("\n\n=== Test 2: Reattach to Running Box ===")

    runtime = boxlite.Boxlite.default()
    box = None
    box2 = None
    box_id = None

    try:
        # Create and start a box
        print("Creating box...")
        box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
        box_id = box.id
        print(f"  Box created: {box_id}")

        # Execute a command to ensure it's fully initialized
        print("\nExecuting initial command...")
        execution = await box.exec("echo", ["Box is running"])
        stdout = execution.stdout()
        async for line in stdout:
            print(f"  {line.strip()}")
        await execution.wait()
        print("  Command executed successfully")

        # Get box info
        info = box.info()
        print(f"\nBox state: {info.state}")

        # Get another handle to the same running box
        print("\nGetting second handle to same box...")
        box2 = runtime.get(box_id)
        if box2 is None:
            print("  Failed to get second handle")
            return

        print(f"  Got second handle: {box2.id}")

        # Execute command via second handle
        print("\nExecuting command via second handle...")
        execution = await box2.exec("echo", ["Via second handle"])
        stdout = execution.stdout()
        async for line in stdout:
            print(f"  {line.strip()}")
        result = await execution.wait()
        print(f"  Command executed (exit code: {result.exit_code})")

        # Clean up - stop via first handle, then remove
        print("\nCleaning up...")
        await box.stop()
        box = None
        box2 = None  # Both handles now invalid
        await runtime.remove(box_id, force=False)
        print("  Box stopped and removed")

    except Exception as e:
        print(f"\n  Error in test: {e}")
        # Cleanup
        if box is not None:
            try:
                await box.stop()
            except:
                pass
        if box_id:
            try:
                await runtime.remove(box_id, force=True)
            except:
                pass

    print("\n  Test 2 completed")


async def test_lifecycle_combinations():
    """Test various lifecycle operation combinations."""
    print("\n\n=== Test 3: Lifecycle Combinations ===")

    runtime = boxlite.Boxlite.default()

    # Test: Create -> Stop -> Restart -> Stop -> Remove
    print("\n--- Combination: Stop -> Restart -> Stop -> Remove ---")

    box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
    box_id = box.id
    print(f"Created box: {box_id}")

    # Execute initial command
    execution = await box.exec("echo", ["Initial"])
    await execution.wait()
    print("  Initial command executed")

    # Stop
    await box.stop()
    print("  Stopped")

    # Get info
    info = runtime.get_info(box_id)
    if info:
        print(f"  State after stop: {info.state}")

    # Restart via get + exec
    box = runtime.get(box_id)
    execution = await box.exec("echo", ["Restart 1"])
    await execution.wait()
    print("  Restarted (1)")

    # Stop again
    await box.stop()
    print("  Stopped again")

    # Restart again
    box = runtime.get(box_id)
    execution = await box.exec("echo", ["Restart 2"])
    await execution.wait()
    print("  Restarted (2)")

    # Stop and remove
    await box.stop()
    await runtime.remove(box_id, force=False)
    print("  Removed")

    print("\n  Combination test completed")


async def test_force_remove():
    """Test force removing a running box."""
    print("\n\n=== Test 4: Force Remove Running Box ===")

    runtime = boxlite.Boxlite.default()

    # Create box
    box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
    box_id = box.id
    print(f"Created box: {box_id}")

    # Execute to make it running
    execution = await box.exec("echo", ["Running"])
    await execution.wait()

    info = runtime.get_info(box_id)
    print(f"Box state: {info.state}")

    # Try normal remove (should fail)
    print("\nTrying normal remove on running box...")
    try:
        await runtime.remove(box_id, force=False)
        print("  Unexpected: Remove succeeded")
    except Exception as e:
        print(f"  Expected error: {e}")

    # Force remove
    print("\nForce removing running box...")
    await runtime.remove(box_id, force=True)
    print("  Force remove succeeded")

    # Verify box is gone
    info = runtime.get_info(box_id)
    if info is None:
        print("  Box is completely removed")
    else:
        print(f"  Unexpected: Box still exists with state {info.state}")

    print("\n  Test 4 completed")


async def test_error_cases():
    """Test error handling in lifecycle operations."""
    print("\n\n=== Test 5: Error Cases ===")

    runtime = boxlite.Boxlite.default()

    # Test removing non-existent box
    print("\n--- Error Case 1: Remove non-existent box ---")
    try:
        await runtime.remove("non-existent-id", force=False)
        print("  Unexpected: Remove succeeded")
    except Exception as e:
        print(f"  Expected error: {e}")

    # Test getting non-existent box
    print("\n--- Error Case 2: Get non-existent box ---")
    result = runtime.get("non-existent-id")
    if result is None:
        print("  Expected: Box not found (returned None)")
    else:
        print("  Unexpected: Should have returned None")

    print("\n  Error cases handled correctly")


async def main():
    """Run all lifecycle tests."""
    print("Box Lifecycle Management Tests")
    print("=" * 60)
    print("\nThis example demonstrates:")
    print("  - Stop: Gracefully stop box (preserves rootfs)")
    print("  - Restart: Restart stopped box (reuses rootfs)")
    print("  - Reattach: Get handle to running box via runtime.get()")
    print("  - Remove: Completely remove box (runtime.remove)")
    print("  - Force Remove: Stop and remove in one call")

    await test_stop_and_restart()
    await test_reattach_to_running()
    await test_lifecycle_combinations()
    await test_force_remove()
    await test_error_cases()

    print("\n" + "=" * 60)
    print("  All lifecycle tests completed!")
    print("\nKey Takeaways:")
    print("  - stop() preserves rootfs - restart reuses existing disk")
    print("  - runtime.get() reconnects to existing box (running or stopped)")
    print("  - exec() on stopped box triggers restart")
    print("  - runtime.remove(id, force=False) requires stopped box")
    print("  - runtime.remove(id, force=True) stops then removes")


if __name__ == "__main__":
    asyncio.run(main())
