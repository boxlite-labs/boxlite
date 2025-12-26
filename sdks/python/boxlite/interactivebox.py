"""
InteractiveBox - Interactive terminal sessions with PTY support.

Provides automatic PTY-based interactive sessions, similar to `docker exec -it`.
"""

import asyncio
import logging
import os
import sys
import termios
import tty
from typing import Optional, TYPE_CHECKING

if TYPE_CHECKING:
    from .runtime import Boxlite

# Configure logger
logger = logging.getLogger("boxlite.interactivebox")


class InteractiveBox:
    """
    Interactive box with automatic PTY and terminal forwarding.

    When used as a context manager, automatically:
    1. Auto-detects terminal size (like Docker)
    2. Starts a shell with PTY
    3. Sets local terminal to cbreak mode
    4. Forwards stdin/stdout bidirectionally
    5. Restores terminal mode on exit

    Example:
        async with InteractiveBox(image="alpine:latest") as box:
            # You're now in an interactive shell!
            # Type commands, see output in real-time
            # Type "exit" to close
            pass
    """

    def __init__(
            self,
            image: str,
            shell: str = "/bin/sh",
            tty: Optional[bool] = None,
            memory_mib: Optional[int] = None,
            cpus: Optional[int] = None,
            runtime: Optional['Boxlite'] = None,
            name: Optional[str] = None,
            auto_remove: bool = True,
            **kwargs
    ):
        """
        Create interactive box.

        Args:
            image: Container image to use
            shell: Shell to run (default: /bin/sh)
            tty: Control terminal I/O forwarding behavior:
                - None (default): Auto-detect - forward I/O if stdin is a TTY
                - True: Force I/O forwarding (manual interactive mode)
                - False: No I/O forwarding (programmatic control only)
            memory_mib: Memory limit in MiB
            cpus: Number of CPU cores
            runtime: Optional runtime instance (uses global default if None)
            name: Optional name for the box (must be unique)
            auto_remove: Remove box when stopped (default: True)
            **kwargs: Additional configuration options (working_dir, env)
        """
        try:
            from .boxlite import Boxlite, BoxOptions
        except ImportError as e:
            raise ImportError(
                f"BoxLite native extension not found: {e}. "
                "Please install with: pip install boxlite"
            )

        # Use provided runtime or get default
        if runtime is None:
            self._runtime = Boxlite.default()
        else:
            self._runtime = runtime

        # Create box options
        box_opts = BoxOptions(
            image=image,
            cpus=cpus,
            memory_mib=memory_mib,
            auto_remove=auto_remove,
            **kwargs
        )

        # Create box directly (no SimpleBox wrapper)
        self._box = self._runtime.create(box_opts, name=name)

        # Store interactive config
        self._shell = shell
        self._env = kwargs.get('env', [])

        # Determine TTY mode: None = auto-detect, True = force, False = disable
        if tty is None:
            self._tty = sys.stdin.isatty()
        else:
            self._tty = tty

        # Interactive state
        self._old_tty_settings = None
        self._io_task = None
        self._execution = None
        self._stdin = None
        self._stdout = None
        self._stderr = None
        self._exited = None  # Event to signal process exit

    @property
    def id(self) -> str:
        """Get the box ID."""
        return self._box.id

    async def __aenter__(self):
        """Start box and enter interactive TTY session."""
        # Start box directly
        await self._box.__aenter__()

        # Start shell with PTY
        self._execution = await self._start_interactive_shell()

        # Get stdin/stdout/stderr ONCE (can only be called once due to .take())
        self._stdin = self._execution.stdin()
        self._stdout = self._execution.stdout()
        self._stderr = self._execution.stderr()

        # Only set cbreak mode and start forwarding if tty=True
        if self._tty:
            stdin_fd = sys.stdin.fileno()
            self._old_tty_settings = termios.tcgetattr(stdin_fd)
            tty.setraw(sys.stdin.fileno(), when=termios.TCSANOW)

            # Create exit event for graceful shutdown
            self._exited = asyncio.Event()

            # Start bidirectional I/O forwarding using gather (more Pythonic)
            self._io_task = asyncio.gather(
                self._forward_stdin(),
                self._forward_output(),
                self._forward_stderr(),
                self._wait_for_exit(),
                return_exceptions=True
            )
        else:
            # No I/O forwarding, just wait for execution
            self._io_task = self._wait_for_exit()

        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        # Restore terminal settings
        if self._old_tty_settings is not None:
            try:
                termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, self._old_tty_settings)
            except Exception as e:
                logger.error(f"Caught exception on TTY settings: {e}")

        """Exit interactive session and restore terminal."""
        # Wait for I/O task to complete (or cancel if needed)
        if hasattr(self, '_io_task') and self._io_task is not None:
            try:
                # Give it a moment to finish naturally
                await asyncio.wait_for(self._io_task, timeout=3)
                logger.info("Closing interactive shell (I/O tasks finished).")
                self._io_task = None
            except asyncio.TimeoutError:
                # If it doesn't finish, that's ok - box is shutting down anyway
                logger.error("Timeout waiting for I/O tasks to finish, cancelling...")
            except Exception as e:
                # Ignore other exceptions during cleanup
                logger.error(f"Caught exception on exit: {e}")

        # Shutdown box directly
        return await self._box.__aexit__(exc_type, exc_val, exc_tb)

    async def wait(self):
        await self._execution.wait()

    async def _start_interactive_shell(self):
        """Start shell with PTY (internal)."""
        # Execute shell with PTY using simplified boolean API
        # Terminal size is auto-detected (like Docker)
        execution = await self._box.exec(
            self._shell,
            args=[],
            env=self._env,
            tty=True,  # Simple boolean - auto-detects terminal size
        )

        return execution

    async def _forward_stdin(self):
        """Forward stdin to PTY (internal)."""
        try:
            if self._stdin is None:
                return

            # Forward stdin in chunks
            loop = asyncio.get_event_loop()
            while not self._exited.is_set():
                # Read from stdin with timeout to check exit event
                try:
                    read_task = loop.run_in_executor(None, os.read, sys.stdin.fileno(), 1024)
                    # Wait for either stdin data or exit event
                    done, pending = await asyncio.wait(
                        [asyncio.ensure_future(read_task),
                         asyncio.ensure_future(self._exited.wait())],
                        return_when=asyncio.FIRST_COMPLETED
                    )

                    # Cancel pending tasks
                    for task in pending:
                        task.cancel()

                    # Check if we exited
                    if self._exited.is_set():
                        logger.info("Closing interactive shell (stdin forwarding).")
                        break

                    # Get the data from completed read task
                    for task in done:
                        if task.exception() is None:
                            data = task.result()
                            if isinstance(data, bytes) and data:
                                await self._stdin.send_input(data)
                            elif not data:
                                # EOF
                                return

                except asyncio.CancelledError:
                    break

        except asyncio.CancelledError:
            logger.info("Cancelling interactive shell (stdin forwarding).")
        except Exception as e:
            logger.error(f"Caught exception on stdin: {e}")

    async def _forward_output(self):
        """Forward PTY output to stdout (internal)."""
        try:
            if self._stdout is None:
                return

            # Forward all output to stdout
            async for chunk in self._stdout:
                # Write directly to stdout (bypass print buffering)
                if isinstance(chunk, bytes):
                    sys.stdout.buffer.write(chunk)
                else:
                    sys.stdout.buffer.write(chunk.encode('utf-8', errors='replace'))
                sys.stdout.buffer.flush()

            logger.info("\nOutput forwarding ended.")

        except asyncio.CancelledError:
            logger.error("Cancelling interactive shell (stdout forwarding).")
        except Exception as e:
            logger.error(f"\nError forwarding output: {e}", file=sys.stderr)

    async def _forward_stderr(self):
        """Forward PTY stderr to stderr (internal)."""
        try:
            if self._stderr is None:
                return

            # Forward all error output to stderr
            async for chunk in self._stderr:
                # Write directly to stderr (bypass print buffering)
                if isinstance(chunk, bytes):
                    sys.stderr.buffer.write(chunk)
                else:
                    sys.stderr.buffer.write(chunk.encode('utf-8', errors='replace'))
                sys.stderr.buffer.flush()

            logger.info("\nStderr forwarding ended.")

        except asyncio.CancelledError:
            logger.error("Cancelling interactive shell (stderr forwarding).")
        except Exception as e:
            logger.error(f"\nError forwarding stderr: {e}", file=sys.stderr)

    async def _wait_for_exit(self):
        """Wait for the shell to exit (internal)."""
        try:
            await self._execution.wait()
        except Exception:
            pass  # Ignore errors, cleanup will happen in __aexit__
        finally:
            # Signal other tasks to stop
            if self._exited:
                self._exited.set()
