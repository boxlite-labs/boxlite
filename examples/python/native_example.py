#!/usr/bin/env python3
"""
Native API Example - Low-Level Box API

Demonstrates the native boxlite API (Rust FFI layer):
- Boxlite runtime initialization and management
- Box lifecycle (create, exec, shutdown, remove)
- Execution streaming (stdout/stderr)
- Runtime and box metrics
- Info and listing operations
"""

import asyncio
import tempfile

import boxlite


async def example_default_runtime():
    """Example 1: Using default runtime."""
    print("\n=== Example 1: Default Runtime ===")

    # Get default runtime (created lazily)
    runtime = boxlite.Boxlite.default()
    print(f"✓ Default runtime: {runtime}")

    # Create a box
    box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
    print(f"✓ Box created: {box.id}")

    # Execute command
    execution = await box.exec("echo", ["Hello from default runtime"])
    stdout = execution.stdout()

    print("Output:")
    async for line in stdout:
        print(f"  {line.encode('utf-8', errors='replace').strip()}")

    exec_result = await execution.wait()
    print(
        f"✓ Exit code: {exec_result.exit_code}s")

    # Shutdown box
    await box.stop()
    print("✓ Box shut down")


async def example_custom_runtime():
    """Example 2: Custom runtime with options."""
    print("\n\n=== Example 2: Custom Runtime ===")

    # Create runtime with custom home directory
    temp_dir = tempfile.mkdtemp(prefix="boxlite-")
    options = boxlite.Options(home_dir=temp_dir)
    runtime = boxlite.Boxlite(options)
    print(f"✓ Custom runtime with home_dir: {temp_dir}")

    # Create box with resource limits
    box_opts = boxlite.BoxOptions(
        image="alpine:latest",
        name="test-box",
        cpus=2,
        memory_mib=512,
        volumes=[("/host/data", "/guest/data", False)],
        ports=[(8080, 80)],
    )
    box = runtime.create(box_opts)
    print(f"✓ Box created with limits: {box.id}")

    # Get box info
    info = box.info()
    print(f"✓ Box info:")
    print(f"  ID: {info.id}")
    print(f"  State: {info.state}")
    print(f"  Image: {info.image}")
    print(f"  CPUs: {info.cpus}")
    print(f"  Memory: {info.memory_mib} MiB")
    print(f"  Transport: {info.transport}")
    print(f"  Created: {info.created_at}")

    await box.stop()
    print("✓ Box shut down")


async def example_streaming_execution():
    """Example 3: Streaming stdout and stderr."""
    print("\n\n=== Example 3: Streaming Execution ===")

    runtime = boxlite.Boxlite.default()
    box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
    print(f"✓ Box created: {box.id}")

    # Execute command that produces both stdout and stderr
    execution = await box.exec(
        "sh",
        ["-c", "echo 'to stdout' && echo 'to stderr' >&2 && echo 'more stdout'"]
    )

    # Get both streams
    exec_id = execution.id
    print(f"✓ Execution ID: {exec_id}")

    stdout = execution.stdout()
    stderr = execution.stderr()

    print("\nStdout stream:")
    async for line in stdout:
        print(f"  stdout: {line.encode('utf-8', errors='replace').strip()}")

    print("\nStderr stream:")
    async for line in stderr:
        print(f"  stderr: {line.encode('utf-8', errors='replace').strip()}")

    exec_result = await execution.wait()
    print(
        f"\n✓ Exit code: {exec_result.exit_code}s")

    await box.stop()


async def example_environment_variables():
    """Example 4: Environment variables."""
    print("\n\n=== Example 4: Environment Variables ===")

    runtime = boxlite.Boxlite.default()
    box = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        env=[("USER", "alice"), ("PROJECT", "boxlite")]
    ))
    print(f"✓ Box created with env vars: {box.id}")

    # Execute with additional env vars
    execution = await box.exec(
        "env",
        None,
        [("CUSTOM", "value"), ("FOO", "bar")]
    )

    stdout = execution.stdout()
    print("\nEnvironment variables:")
    async for line in stdout:
        line = line.strip()
        if any(key in line for key in ["USER=", "PROJECT=", "CUSTOM=", "FOO="]):
            print(f"  {line}")

    await execution.wait()
    await box.stop()


async def example_box_metrics():
    """Example 5: Box metrics."""
    print("\n\n=== Example 5: Box Metrics ===")

    runtime = boxlite.Boxlite.default()
    box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
    print(f"✓ Box created: {box.id}")

    # Execute some commands
    for i in range(3):
        execution = await box.exec("echo", [f"Command {i + 1}"])
        stdout = execution.stdout()
        async for line in stdout:
            pass  # Consume output
        await execution.wait()

    # Get box metrics
    metrics = await box.metrics()
    print(f"\n✓ Box metrics:")
    print(f"  Commands executed: {metrics.commands_executed_total}")
    print(f"  Exec errors: {metrics.exec_errors_total}")
    print(f"  Bytes sent: {metrics.bytes_sent_total}")
    print(f"  Bytes received: {metrics.bytes_received_total}")
    print(f"  Create duration: {metrics.total_create_duration_ms} ms")
    print(f"  Boot duration: {metrics.guest_boot_duration_ms} ms")
    if metrics.cpu_percent is not None:
        print(f"  CPU usage: {metrics.cpu_percent:.2f}%")
    if metrics.memory_bytes is not None:
        print(f"  Memory: {metrics.memory_bytes / 1024 / 1024:.2f} MiB")
    if metrics.network_bytes_sent is not None:
        print(f"  Network sent: {metrics.network_bytes_sent} bytes")
    if metrics.network_bytes_received is not None:
        print(f"  Network received: {metrics.network_bytes_received} bytes")
    if metrics.network_tcp_connections is not None:
        print(f"  TCP connections: {metrics.network_tcp_connections}")
    if metrics.network_tcp_errors is not None:
        print(f"  TCP errors: {metrics.network_tcp_errors}")

    await box.stop()


async def example_runtime_metrics():
    """Example 6: Runtime metrics."""
    print("\n\n=== Example 6: Runtime Metrics ===")

    runtime = boxlite.Boxlite.default()

    # Create and run multiple boxes
    boxes = []
    for i in range(2):
        box = runtime.create(boxlite.BoxOptions(image="alpine:latest"))
        boxes.append(box)
        print(f"✓ Box {i + 1} created: {box.id}")

        # Execute a command
        execution = await box.exec("echo", [f"Box {i + 1}"])
        stdout = execution.stdout()
        async for line in stdout:
            pass  # Consume output
        await execution.wait()

    # Get runtime metrics
    metrics = runtime.metrics()
    print(f"\n✓ Runtime metrics:")
    print(f"  Total boxes created: {metrics.boxes_created_total}")
    print(f"  Failed boxes: {metrics.boxes_failed_total}")
    print(f"  Running boxes: {metrics.num_running_boxes}")
    print(f"  Total commands: {metrics.total_commands_executed}")
    print(f"  Total errors: {metrics.total_exec_errors}")

    # Cleanup
    for box in boxes:
        await box.stop()
    print("\n✓ All boxes shut down")


async def example_list_and_info():
    """Example 7: Listing and getting box info."""
    print("\n\n=== Example 7: List and Get Info ===")

    runtime = boxlite.Boxlite.default()

    # Create multiple boxes
    boxes = []
    for i in range(3):
        box = runtime.create(boxlite.BoxOptions(
            image="alpine:latest",
            name=f"test-box-{i}"
        ))
        boxes.append(box)
        print(f"✓ Box {i + 1} created: {box.id}")

    # List all boxes
    all_boxes = runtime.list()
    print(f"\n✓ Total boxes: {len(all_boxes)}")
    for info in all_boxes[-3:]:  # Show last 3
        print(f"  - {info.id}: {info.state} ({info.image})")

    # Get specific box info
    if boxes:
        info = boxes[0].info()
        print(f"\n✓ Box info for {boxes[0].id}:")
        print(f"  State: {info.state}")
        print(f"  Image: {info.image}")
        print(f"  CPUs: {info.cpus}")
        print(f"  Memory: {info.memory_mib} MiB")

    # Cleanup - shutdown and remove
    for box in boxes:
        await box.stop()
        await runtime.remove(box.id, force=False)
        print(f"✓ Removed box: {box.id}")


async def example_execution_kill():
    """Example 8: Kill running execution."""
    print("\n\n=== Example 8: Kill Execution ===")

    runtime = boxlite.Boxlite.default()
    box = runtime.create(
        boxlite.BoxOptions(image="alpine:latest", env=[("RUST_LOG", "boxlite=trace,box=trace")]))
    print(f"✓ Box created: {box.id}")

    # Start a long-running command
    execution = await box.exec("sh", ["-c", "sleep 100; echo done"])
    exec_id = execution.id
    print(f"✓ Started long-running execution: {str(exec_id)}")

    # Wait a bit
    await asyncio.sleep(0.5)

    # Kill the execution
    await execution.kill()
    print("✓ Execution killed")

    # Wait for exit
    exec_result = await execution.wait()
    print(
        f"✓ Exit code after kill: {exec_result.exit_code}s")

    await box.stop()


async def example_context_manager():
    """Example 9: Using Box as async context manager."""
    print("\n\n=== Example 9: Context Manager ===")

    runtime = boxlite.Boxlite.default()

    # Box automatically shuts down when exiting context
    async with runtime.create(boxlite.BoxOptions(image="alpine:latest")) as box:
        print(f"✓ Box created in context: {box.id}")

        execution = await box.exec("echo", ["Hello from context manager"])
        stdout = execution.stdout()

        async for line in stdout:
            print(f"  {line.strip()}")

        exec_result = await execution.wait()
        print(
            f"✓ Execution completed: code={exec_result.exit_code}s")

    print("✓ Box automatically shut down on context exit")


async def example_working_directory():
    """Example 10: Working directory and port mappings."""
    print("\n\n=== Example 10: Working Directory & Ports ===")

    runtime = boxlite.Boxlite.default()

    box = runtime.create(boxlite.BoxOptions(
        image="alpine:latest",
        working_dir="/tmp",
        ports=[(8080, 80)]  # host:container
    ))
    print(f"✓ Box created: {box.id}")

    # Verify working directory
    execution = await box.exec("pwd", None, None)
    stdout = execution.stdout()

    print("\nWorking directory:")
    async for line in stdout:
        print(f"  {line.encode('utf-8', errors='replace').strip()}")

    await execution.wait()

    # Get box info to see port mappings
    info = box.info()
    print(f"✓ Box configuration verified")

    await box.stop()


async def main():
    """Run all native API examples."""
    print("Native Box API Examples - Low-Level Rust FFI Layer")
    print("=" * 60)

    await example_default_runtime()
    await example_custom_runtime()
    await example_streaming_execution()
    await example_environment_variables()
    await example_box_metrics()
    await example_runtime_metrics()
    await example_list_and_info()
    await example_execution_kill()
    await example_context_manager()
    await example_working_directory()

    print("\n" + "=" * 60)
    print("✓ All native API examples completed!")
    print("\nKey Takeaways:")
    print("  • Boxlite.default() for most use cases (global runtime)")
    print("  • Box.exec() returns Execution handle for streaming")
    print("  • Separate stdout/stderr streams for real-time output")
    print("  • Comprehensive metrics (runtime and per-box)")
    print("  • List/get_info for box management")
    print("  • Context manager for automatic cleanup")
    print("  • Full control over resources (CPU, memory, env, ports)")


if __name__ == "__main__":
    asyncio.run(main())
