#!/usr/bin/env python3
"""
SimpleBox Example - Foundation for Custom Containers

Demonstrates core BoxLite features using SimpleBox directly:
- Command execution with results
- Separate stdout and stderr handling
- Environment variables and working directory
- Error handling and exit codes
- Multiple commands in same container
- Real-world use case: Running a data processing pipeline
"""

import asyncio
import logging

import boxlite

logger = logging.getLogger("simplebox_example")


def setup_logging():
    """Configure stdout logging for the example."""
    logging.basicConfig(
        level=logging.ERROR,
        format="%(asctime)s [%(levelname)s] %(message)s",
        handlers=[logging.StreamHandler(sys.stdout)],
    )


async def example_basic():
    """Example 1: Basic command execution."""
    print("\n=== Example 1: Basic Command Execution ===")

    async with boxlite.SimpleBox(image="python:alpine") as box:
        print(f"✓ Container started: {box.id}")

        # Execute command and get result
        print("\nContainer filesystem:")
        result = await box.exec("ls", "-lh", "/")
        print(result.stdout)

        if result.stderr:
            print(f"Stderr: {result.stderr}")
        print(f"Exit code: {result.exit_code}")


async def example_stdout_stderr():
    """Example 2: Separate stdout and stderr."""
    print("\n\n=== Example 2: Separate stdout and stderr ===")

    async with boxlite.SimpleBox(image="python:alpine") as box:
        print(f"✓ Container started: {box.id}")

        # Command that produces both stdout and stderr
        print("\nRunning command with both stdout and stderr:")
        result = await box.exec('sh', '-c', 'echo "to stdout" && echo "to stderr" >&2')

        print(f"Exit code: {result.exit_code}")
        print(f"Stdout: '{result.stdout.strip()}'")
        print(f"Stderr: '{result.stderr.strip()}'")


async def example_environment():
    """Example 3: Environment variables."""
    print("\n\n=== Example 3: Environment Variables ===")

    async with boxlite.SimpleBox(image="python:alpine") as box:
        print(f"✓ Container started: {box.id}")

        # Execute with custom environment variables
        print("\nSetting FOO=bar and BAZ=qux:")
        result = await box.exec('env', env={'FOO': 'bar', 'BAZ': 'qux'})

        print(f"Exit code: {result.exit_code}")
        print("Custom environment variables:")
        for line in result.stdout.split('\n'):
            if 'FOO=' in line or 'BAZ=' in line:
                print(f"  {line}")


async def example_working_directory():
    """Example 4: Working directory."""
    print("\n\n=== Example 4: Working Directory ===")

    async with boxlite.SimpleBox(
            image="python:alpine",
            working_dir="/tmp",
            env=[("USER", "alice"), ("PROJECT", "data-pipeline")]
    ) as box:
        print(f"✓ Container with custom config: {box.id}")

        # Check working directory
        print("\nCurrent directory:")
        result = await box.exec("pwd")
        print(f"  {result.stdout.strip()}")

        # Check environment
        print("\nEnvironment variables:")
        result = await box.exec("env")
        for line in result.stdout.split('\n'):
            if "USER=" in line or "PROJECT=" in line:
                print(f"  {line}")


async def example_error_handling():
    """Example 5: Error handling."""
    print("\n\n=== Example 5: Error Handling ===")

    async with boxlite.SimpleBox(image="python:alpine") as box:
        print(f"✓ Container started: {box.id}")

        # Command that fails
        print("\nRunning command that fails:")
        result = await box.exec('false')

        if result.exit_code != 0:
            print(f"✓ Command failed as expected with exit code: {result.exit_code}")
        else:
            print("Command succeeded")

        # Command that succeeds
        print("\nRunning command that succeeds:")
        result = await box.exec('true')

        if result.exit_code == 0:
            print(f"✓ Command succeeded with exit code: {result.exit_code}")
        else:
            print(f"Command failed with exit code: {result.exit_code}")


async def example_pipeline():
    """Example 6: Real-world data processing pipeline."""
    print("\n\n=== Example 6: Data Processing Pipeline ===")

    async with boxlite.SimpleBox(image="python:alpine") as box:
        print(f"✓ Running data pipeline in: {box.id}")

        # Step 1: Generate sample data
        print("\n1. Generating sample data...")
        result = await box.exec(
            "python", "-c",
            "import json; data = [{'id': i, 'value': i*2} for i in range(5)]; "
            "print(json.dumps(data, indent=2))"
        )
        print(result.stdout)

        # Step 2: Process data with transformation
        print("2. Processing data...")
        result = await box.exec(
            "python", "-c",
            "import json; data = [{'id': i, 'value': i*2} for i in range(5)]; "
            "total = sum(item['value'] for item in data); "
            "print(f'Total: {total}')"
        )
        print(result.stdout)

        # Step 3: Verify system resources
        print("3. Container resources:")
        result = await box.exec("free", "-h")
        print(result.stdout)


async def main():
    """Run all examples."""
    print("SimpleBox Examples - Foundation for Custom Containers")
    print("=" * 60)

    await example_basic()
    await example_stdout_stderr()
    await example_environment()
    await example_working_directory()
    await example_error_handling()
    await example_pipeline()

    print("\n" + "=" * 60)
    print("✓ All examples completed!")
    print("\nKey Takeaways:")
    print("  • Simple exec() API returns ExecResult with exit_code, stdout, stderr")
    print("  • Stdout and stderr are separated for clarity")
    print("  • Environment variables can be set per-exec or per-box")
    print("  • Working directory can be customized")
    print("  • Exit codes enable proper error handling")
    print("  • Perfect for building custom specialized boxes")


if __name__ == "__main__":
    setup_logging()
    logger.info("Python logging configured; runtime logs will emit to stdout.")
    asyncio.run(main())
