#!/usr/bin/env python3
"""
Cross-Process Box Management Example

Demonstrates cross-process box operations:
- Reattach: Connect to a running box started by another process
- Restart: Restart a stopped box from a different process

IMPORTANT: This example uses Boxlite(Options()) instead of Boxlite.default()
to ensure proper lock release between processes. The default() uses a static
singleton that holds the lock forever.
"""

import asyncio
import gc
import os
import subprocess
import sys

import boxlite


async def _subprocess_start_box():
    """Helper: Start a box in subprocess and exit (box keeps running)."""
    # Use Options() to create a droppable runtime
    runtime = boxlite.Boxlite(boxlite.Options())

    box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
    box_id = box.id

    # Execute command to ensure it's fully initialized
    execution = await box.exec("echo", ["initialized"])
    await execution.wait()

    # Print box_id for parent to capture
    print(f"BOX_ID:{box_id}")
    sys.stdout.flush()

    # Exit without stopping - box keeps running!
    # Runtime will be dropped, releasing the lock


async def _subprocess_start_and_stop_box():
    """Helper: Start a box, stop it, and exit (box is stopped)."""
    # Use Options() to create a droppable runtime
    runtime = boxlite.Boxlite(boxlite.Options())

    box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
    box_id = box.id

    # Execute command to ensure it's fully initialized
    execution = await box.exec("echo", ["initialized"])
    await execution.wait()

    # Stop the box
    await box.stop()

    # Print box_id for parent to capture
    print(f"BOX_ID:{box_id}")
    sys.stdout.flush()

    # Exit - box is stopped but still exists in DB
    # Runtime will be dropped, releasing the lock


async def test_cross_process_reattach():
    """Test attaching to a running box from a different process."""
    print("\n=== Test 1: Cross-Process Reattach ===")
    print("This tests connecting to a box started by another process.\n")

    box_id = None
    runtime = None

    try:
        # Spawn subprocess to start a box
        # NOTE: Parent must NOT hold runtime lock while subprocess runs
        print("Spawning subprocess to start box...")
        env = os.environ.copy()
        result = subprocess.run(
            [sys.executable, __file__, "--subprocess-start"],
            capture_output=True,
            text=True,
            env=env,
            timeout=60
        )

        if result.returncode != 0:
            print(f"  Subprocess failed: {result.stderr}")
            return

        # Parse box_id from output
        for line in result.stdout.splitlines():
            if line.startswith("BOX_ID:"):
                box_id = line.split(":", 1)[1].strip()
                break

        if not box_id:
            print(f"  Failed to get box_id from subprocess")
            print(f"  stdout: {result.stdout}")
            return

        print(f"  Subprocess started box: {box_id}")
        print("  Subprocess exited (box still running)")

        # Create a NEW runtime (not default!) so it can be properly released
        # Using Options() creates a droppable runtime
        runtime = boxlite.Boxlite(boxlite.Options())

        # Verify box is still running
        info = runtime.get_info(box_id)
        if info is None:
            print(f"  Error: Box not found")
            return
        print(f"  Box state in DB: {info.state}")

        # Now attach from this process
        print("\nAttaching to running box from this process...")
        box = runtime.get(box_id)
        if box is None:
            print("  Failed to get box handle")
            return

        print(f"  Got handle: {box.id}")

        # Execute command via reattached handle
        print("\nExecuting command via reattached handle...")
        execution = await box.exec("echo", ["Hello from parent process!"])
        stdout = execution.stdout()
        async for line in stdout:
            print(f"  {line.strip()}")
        result = await execution.wait()
        print(f"  Exit code: {result.exit_code}")

        print("\n  SUCCESS: Cross-process reattach works!")

    except subprocess.TimeoutExpired:
        print("  Subprocess timed out")
    except Exception as e:
        print(f"\n  Error: {e}")
        import traceback
        traceback.print_exc()

    finally:
        # Cleanup using existing runtime
        if box_id and runtime is not None:
            try:
                await runtime.remove(box_id, force=True)
                print("\n  Box cleaned up")
            except:
                pass

        # Release runtime to free the lock for next test
        # Delete reference and force garbage collection
        if runtime is not None:
            del runtime
            gc.collect()
            await asyncio.sleep(0.1)

    print("\n  Test 1 completed")


async def test_cross_process_restart():
    """Test restarting a stopped box from a different process."""
    print("\n\n=== Test 2: Cross-Process Restart ===")
    print("This tests restarting a box stopped by another process.\n")

    box_id = None
    runtime = None

    try:
        # Spawn subprocess to start and stop a box
        print("Spawning subprocess to start and stop box...")
        env = os.environ.copy()
        result = subprocess.run(
            [sys.executable, __file__, "--subprocess-start-stop"],
            capture_output=True,
            text=True,
            env=env,
            timeout=60
        )

        if result.returncode != 0:
            print(f"  Subprocess failed: {result.stderr}")
            return

        # Parse box_id from output
        for line in result.stdout.splitlines():
            if line.startswith("BOX_ID:"):
                box_id = line.split(":", 1)[1].strip()
                break

        if not box_id:
            print(f"  Failed to get box_id from subprocess")
            print(f"  stdout: {result.stdout}")
            return

        print(f"  Subprocess created and stopped box: {box_id}")
        print("  Subprocess exited")

        # Create a NEW runtime (not default!) so it can be properly released
        runtime = boxlite.Boxlite(boxlite.Options())

        # Verify box is stopped
        info = runtime.get_info(box_id)
        if info is None:
            print(f"  Error: Box not found")
            return
        print(f"  Box state in DB: {info.state}")

        # Now restart from this process
        print("\nRestarting stopped box from this process...")
        box = runtime.get(box_id)
        if box is None:
            print("  Failed to get box handle")
            return

        print(f"  Got handle: {box.id}")

        # Execute command triggers restart
        print("\nExecuting command (triggers restart)...")
        execution = await box.exec("echo", ["Hello from parent process after restart!"])
        stdout = execution.stdout()
        async for line in stdout:
            print(f"  {line.strip()}")
        result = await execution.wait()
        print(f"  Exit code: {result.exit_code}")

        print("\n  SUCCESS: Cross-process restart works!")

    except subprocess.TimeoutExpired:
        print("  Subprocess timed out")
    except Exception as e:
        print(f"\n  Error: {e}")
        import traceback
        traceback.print_exc()

    finally:
        # Cleanup using existing runtime
        if box_id and runtime is not None:
            try:
                await runtime.remove(box_id, force=True)
                print("\n  Box cleaned up")
            except:
                pass

        # Release runtime to free the lock
        if runtime is not None:
            del runtime
            gc.collect()

    print("\n  Test 2 completed")


async def main():
    """Run all cross-process tests."""
    print("Cross-Process Box Management Tests")
    print("=" * 60)
    print("\nThis example demonstrates:")
    print("  - Reattach: Connect to a running box from another process")
    print("  - Restart: Restart a stopped box from another process")
    print("\nNOTE: Uses Boxlite(Options()) instead of Boxlite.default()")
    print("      to ensure proper lock release between processes.")

    await test_cross_process_reattach()
    await test_cross_process_restart()

    print("\n" + "=" * 60)
    print("  All cross-process tests completed!")
    print("\nKey Takeaways:")
    print("  - Use Boxlite(Options()) for cross-process scenarios")
    print("  - Subprocess creates box, parent can reattach/restart")
    print("  - Box state persists in DB across process boundaries")
    print("  - exec() on stopped box triggers automatic restart")


if __name__ == "__main__":
    # Handle subprocess modes
    if len(sys.argv) > 1:
        if sys.argv[1] == "--subprocess-start":
            asyncio.run(_subprocess_start_box())
        elif sys.argv[1] == "--subprocess-start-stop":
            asyncio.run(_subprocess_start_and_stop_box())
        else:
            asyncio.run(main())
    else:
        asyncio.run(main())
