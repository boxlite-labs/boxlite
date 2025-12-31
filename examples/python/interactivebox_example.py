#!/usr/bin/env python3
"""
Simple Interactive Shell - Drop directly into a container shell

This is the simplest example - just like running `docker exec -it container sh`.
Run this script and you'll get an interactive shell where you can type commands.

Usage:
    python examples/python/interactivebox_example.py
"""

import asyncio
import logging
import os
import sys

logger = logging.getLogger("interactivebox_example")


def setup_logging():
    """Configure stdout logging for the example."""
    logging.basicConfig(
        level=logging.ERROR,
        format="%(asctime)s [%(levelname)s] %(message)s",
        handlers=[logging.StreamHandler(sys.stdout)],
    )


async def main():
    print("Starting interactive Alpine container...")
    print("Type 'exit' or press Ctrl+D to quit\n")

    try:
        from boxlite import InteractiveBox

        # This is all you need for an interactive shell!
        term_mode = os.environ.get("TERM", "xterm-256color")
        print(f"Terminal mode: {term_mode}")
        async with InteractiveBox(image="alpine:latest", env=[("TERM", term_mode)], ports=[(28080, 8000)]) as itbox:
            # You're now in an interactive shell
            # Everything you type goes to the container
            # Everything the container outputs comes back to your terminal

            # Wait for the shell to exit
            # The InteractiveBox automatically handles all I/O in background tasks
            await itbox.wait()

    except KeyboardInterrupt:
        print("\n\nInterrupted by Ctrl+C")
    except Exception as e:
        print(f"\nError: {e}", file=sys.stderr)
        import traceback
        traceback.print_exc()
        sys.exit(1)


if __name__ == "__main__":
    setup_logging()
    asyncio.run(main())
