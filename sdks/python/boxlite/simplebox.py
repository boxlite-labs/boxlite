"""
SimpleBox - Foundation for specialized container types.

Provides common functionality for all specialized boxes (CodeBox, BrowserBox, etc.)
"""

import logging
from enum import IntEnum
from typing import Optional

from .exec import ExecResult

logger = logging.getLogger("boxlite.simplebox")

__all__ = ['SimpleBox']


class StreamType(IntEnum):
    """Stream type for command execution output."""
    STDOUT = 1
    STDERR = 2


class SimpleBox:
    """
    Base class for specialized container types.

    This class encapsulates the common patterns:
    1. Async context manager support
    2. Automatic runtime lifecycle management
    3. Stdio blocking mode restoration

    Subclasses should override:
    - _create_box_options(): Return BoxOptions for their specific use case
    - Add domain-specific methods (e.g., CodeBox.run(), BrowserBox.navigate())
    """

    def __init__(
            self,
            image: str,
            memory_mib: Optional[int] = None,
            cpus: Optional[int] = None,
            runtime: Optional['Boxlite'] = None,
            **kwargs
    ):
        """
        Create a specialized box.

        Args:
            image: Container images to use
            memory_mib: Memory limit in MiB
            cpus: Number of CPU cores
            runtime: Optional runtime instance (uses global default if None)
            **kwargs: Additional configuration options
        """
        try:
            from .boxlite import Boxlite
        except ImportError as e:
            raise ImportError(
                f"BoxLite native extension not found: {e}. "
                "Please install with: pip install boxlite"
            )

        # Use provided runtime or get Rust's global default
        if runtime is None:
            self._runtime = Boxlite.default()
        else:
            self._runtime = runtime

        # Create box using subclass-defined options
        try:
            from .boxlite import BoxOptions
        except ImportError as e:
            raise ImportError(
                f"BoxLite native extension not found: {e}. "
                "Please install with: pip install boxlite"
            )

        box_opts = BoxOptions(
            image=image,
            cpus=cpus,
            memory_mib=memory_mib,
            **kwargs
        )
        self._box = self._runtime.create(box_opts)

    async def __aenter__(self):
        """Async context manager entry - delegates to Box.__aenter__."""
        await self._box.__aenter__()
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        """Async context manager exit - delegates to Box.__aexit__ (returns awaitable)."""
        return await self._box.__aexit__(exc_type, exc_val, exc_tb)

    @property
    def id(self) -> str:
        """Get the box ID."""
        return self._box.id

    def info(self):
        """Get box information."""
        return self._box.info()

    async def exec(
            self,
            cmd: str,
            *args: str,
            env: Optional[dict[str, str]] = None,
    ) -> ExecResult:
        """
        Execute a command in the box and return the result.

        Args:
            cmd: Command to execute (e.g., 'ls', 'python')
            *args: Arguments to the command (e.g., '-l', '-a')
            env: Environment variables (default: guest's default environment)

        Returns:
            ExecResult with exit_code and output

        Examples:
            Simple execution::

                result = await box.exec('ls', '-l', '-a')
                print(f"Exit code: {result.exit_code}")
                print(f"Stdout: {result.stdout}")
                print(f"Stderr: {result.stderr}")

            With environment variables::

                result = await box.exec('env', env={'FOO': 'bar'})
                print(result.stdout)
        """

        arg_list = list(args) if args else None
        # Convert env dict to list of tuples if provided
        env_list = list(env.items()) if env else None

        # Execute via Rust (returns PyExecution)
        execution = await self._box.exec(cmd, arg_list, env_list)

        # Get streams from Rust execution
        try:
            stdout = execution.stdout()
        except Exception as e:
            logger.error(f"take stdout err: {e}")
            stdout = None

        try:
            stderr = execution.stderr()
        except Exception as e:
            logger.error(f"take stderr err: {e}")
            stderr = None

        # Collect stdout and stderr separately
        stdout_lines = []
        stderr_lines = []

        # Read stdout
        if stdout:
            logger.debug("collecting stdout")
            try:
                async for line in stdout:
                    if isinstance(line, bytes):
                        stdout_lines.append(line.decode('utf-8', errors='replace'))
                    else:
                        stdout_lines.append(line)
            except Exception as e:
                logger.error(f"collecting stdout err: {e}")
                pass

        # Read stderr
        if stderr:
            logger.debug("collecting stderr")
            try:
                async for line in stderr:
                    if isinstance(line, bytes):
                        stderr_lines.append(line.decode('utf-8', errors='replace'))
                    else:
                        stderr_lines.append(line)
            except Exception as e:
                logger.error(f"collecting stderr err: {e}")
                pass

        # Combine lines
        stdout = ''.join(stdout_lines)
        stderr = ''.join(stderr_lines)

        try:
            exec_result = await execution.wait()
            exit_code = exec_result.exit_code
        except Exception as e:
            logger.error(f"failed to wait execution: {e}")
            exit_code = -1

        logger.debug(f"exec finish, exit_code: {exit_code}")

        return ExecResult(exit_code=exit_code, stdout=stdout, stderr=stderr)

    def shutdown(self):
        """
        Shutdown the box and release resources.

        Note: Usually not needed as context manager handles cleanup.
        """
        self._box.shutdown()
