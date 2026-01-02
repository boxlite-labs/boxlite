#!/usr/bin/env python3
"""
Detach and Auto-Remove Options Example

Demonstrates the lifecycle control options:
1. auto_remove=True (default): Box is automatically removed when stopped
2. auto_remove=False: Box is preserved after stop, can be restarted
3. detach=False (default): Box stops when parent process exits
4. detach=True: Box survives parent process exit

These options are similar to Docker's --rm and -d flags.
"""

import asyncio

import boxlite


async def demo_auto_remove_true():
    """Demo auto_remove=True behavior (default).

    When auto_remove=True:
    - Box is automatically removed when stop() is called
    - No need to call runtime.remove() separately
    - Similar to Docker's --rm flag
    """
    print("=== Demo 1: auto_remove=True (default) ===")
    print("Box will be automatically removed when stopped.\n")

    runtime = boxlite.Boxlite.default()

    # Create box with default auto_remove=True
    print("1. Creating box with auto_remove=True (default)...")
    box = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        auto_remove=True,  # This is the default, shown explicitly for clarity
    ))
    box_id = box.id
    print(f"   Box created: {box_id}")

    # Execute a command
    print("\n2. Executing command...")
    result = await box.exec("echo", ["Hello from auto-remove box!"])
    print(f"   Output: {result.stdout()}")

    # Check box exists before stop
    info = runtime.get_info(box_id)
    print(f"\n3. Box exists before stop: {info is not None}")

    # Stop the box - this will automatically remove it
    print("\n4. Stopping box (auto-remove will trigger)...")
    await box.stop()
    print("   Box stopped")

    # Check box no longer exists
    info = runtime.get_info(box_id)
    print(f"\n5. Box exists after stop: {info is not None}")
    if info is None:
        print("   Box was automatically removed!")

    runtime.close()
    print("\nDemo 1 completed.\n")


async def demo_auto_remove_false():
    """Demo auto_remove=False behavior.

    When auto_remove=False:
    - Box is preserved after stop() is called
    - Box can be restarted using runtime.get() + exec()
    - Must call runtime.remove() to clean up
    """
    print("=== Demo 2: auto_remove=False ===")
    print("Box will be preserved after stop, allowing restart.\n")

    runtime = boxlite.Boxlite.default()

    # Create box with auto_remove=False
    print("1. Creating box with auto_remove=False...")
    box = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        auto_remove=False,  # Preserve box after stop
    ))
    box_id = box.id
    print(f"   Box created: {box_id}")

    # Execute a command
    print("\n2. Executing command...")
    result = await box.exec("echo", ["Hello!"])
    print(f"   Output: {result.stdout()}")

    # Stop the box
    print("\n3. Stopping box...")
    await box.stop()
    print("   Box stopped")

    # Check box still exists
    info = runtime.get_info(box_id)
    print(f"\n4. Box exists after stop: {info is not None}")
    if info:
        print(f"   Box state: {info.state}")

    # Restart by getting handle and executing
    print("\n5. Restarting box (get handle + exec)...")
    restarted_box = runtime.get(box_id)
    if restarted_box:
        result = await restarted_box.exec("echo", ["Restarted!"])
        print(f"   Output: {result.stdout()}")

        # Clean up - stop then remove
        print("\n6. Cleaning up (stop + remove)...")
        await restarted_box.stop()
        await runtime.remove(box_id, force=False)
        print("   Box removed")
    else:
        print("   Failed to get box handle")

    runtime.close()
    print("\nDemo 2 completed.\n")


async def demo_detach_false():
    """Demo detach=False behavior (default).

    When detach=False:
    - Box is tied to parent process lifecycle
    - Box will stop when the parent process exits
    - This is the default behavior to prevent orphan boxes
    """
    print("=== Demo 3: detach=False (default) ===")
    print("Box is tied to parent process - stops when parent exits.\n")

    runtime = boxlite.Boxlite.default()

    # Create box with default detach=False
    print("1. Creating box with detach=False (default)...")
    box = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        detach=False,  # This is the default
        auto_remove=True,
    ))
    box_id = box.id
    print(f"   Box created: {box_id}")
    print("   This box would stop automatically if this process exited.")

    # Execute a command
    print("\n2. Executing command...")
    result = await box.exec("echo", ["I'm tied to parent!"])
    print(f"   Output: {result.stdout()}")

    # Clean up normally for this demo
    print("\n3. Stopping box...")
    await box.stop()
    print("   Box stopped and auto-removed")

    runtime.close()
    print("\nDemo 3 completed.\n")


async def demo_detach_true():
    """Demo detach=True behavior.

    When detach=True:
    - Box runs independently of parent process
    - Box survives if parent process exits
    - Similar to Docker's -d (detach) flag
    - Useful for long-running services
    """
    print("=== Demo 4: detach=True ===")
    print("Box runs independently - survives parent exit.\n")

    runtime = boxlite.Boxlite.default()

    # Create box with detach=True
    print("1. Creating box with detach=True...")
    box = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        detach=True,  # Run independently
        auto_remove=False,  # Keep around for demo
    ))
    box_id = box.id
    print(f"   Box created: {box_id}")
    print("   This box would continue running if this process exited.")
    print("   You could reattach using: runtime.get(box_id)")

    # Execute a command
    print("\n2. Executing command...")
    result = await box.exec("echo", ["I'm detached!"])
    print(f"   Output: {result.stdout()}")

    # Show how to reattach
    print("\n3. Simulating reattach (getting new handle)...")
    new_handle = runtime.get(box_id)
    if new_handle:
        result = await new_handle.exec("echo", ["Via new handle!"])
        print(f"   Output: {result.stdout()}")

    # Clean up for demo
    print("\n4. Cleaning up...")
    await box.stop()
    await runtime.remove(box_id, force=False)
    print("   Box stopped and removed")

    runtime.close()
    print("\nDemo 4 completed.\n")


async def demo_combined_options():
    """Demo combining auto_remove and detach options."""
    print("=== Demo 5: Combined Options ===")
    print("Common option combinations and their use cases.\n")

    runtime = boxlite.Boxlite.default()

    # Combination 1: auto_remove=True, detach=False (default)
    # Use case: Ephemeral sandbox for one-off tasks
    print("1. Ephemeral sandbox (auto_remove=True, detach=False):")
    print("   Use case: One-off code execution, testing")
    box1 = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        auto_remove=True,
        detach=False,
    ))
    result = await box1.exec("echo", ["One-off task"])
    print(f"   Output: {result.stdout()}")
    await box1.stop()
    print("   Box auto-removed on stop\n")

    # Combination 2: auto_remove=False, detach=False
    # Use case: Development/debugging - restart box with same state
    print("2. Development sandbox (auto_remove=False, detach=False):")
    print("   Use case: Iterative development, debugging")
    box2 = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        auto_remove=False,
        detach=False,
    ))
    box2_id = box2.id
    await box2.exec("touch", ["/tmp/dev-file"])
    await box2.stop()
    # Can restart and continue development
    box2_restarted = runtime.get(box2_id)
    result = await box2_restarted.exec("ls", ["/tmp/dev-file"])
    print(f"   File persisted: '/tmp/dev-file' in {result.stdout()}")
    await box2_restarted.stop()
    await runtime.remove(box2_id)
    print("   Box manually removed\n")

    # Combination 3: auto_remove=False, detach=True
    # Use case: Long-running service that survives parent exit
    print("3. Background service (auto_remove=False, detach=True):")
    print("   Use case: Long-running services, daemons")
    box3 = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        auto_remove=False,
        detach=True,
    ))
    box3_id = box3.id
    print(f"   Service box: {box3_id}")
    print("   This box would survive parent process exit")
    await box3.stop()
    await runtime.remove(box3_id)
    print("   Box manually stopped and removed\n")

    runtime.close()
    print("Demo 5 completed.\n")


async def main():
    """Run all demos."""
    print("=" * 60)
    print("BoxLite Detach and Auto-Remove Options Demo")
    print("=" * 60)
    print()
    print("Options explained:")
    print("  auto_remove: Control box cleanup on stop")
    print("    - True (default):  Box removed when stop() called")
    print("    - False:           Box preserved, can restart")
    print()
    print("  detach: Control parent process lifecycle tie")
    print("    - False (default): Box stops when parent exits")
    print("    - True:            Box survives parent exit")
    print()
    print("=" * 60)
    print()

    await demo_auto_remove_true()
    await demo_auto_remove_false()
    await demo_detach_false()
    await demo_detach_true()
    await demo_combined_options()

    print("=" * 60)
    print("All demos completed!")
    print()
    print("Summary:")
    print("  - auto_remove=True (default): Ephemeral boxes, auto-cleanup")
    print("  - auto_remove=False: Persistent boxes, manual cleanup")
    print("  - detach=False (default): Tied to parent, prevents orphans")
    print("  - detach=True: Independent boxes, survives parent exit")
    print("=" * 60)


if __name__ == "__main__":
    asyncio.run(main())
