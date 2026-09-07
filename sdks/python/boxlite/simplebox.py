"""
SimpleBox - Foundation for specialized container types.

Provides common functionality for all specialized boxes (CodeBox, BrowserBox, etc.)
"""

import asyncio
import inspect
import logging
from enum import IntEnum
from typing import TYPE_CHECKING, Optional

from .exec import ExecResult

if TYPE_CHECKING:
    from .boxlite import Boxlite, TunnelForwarder

logger = logging.getLogger("boxlite.simplebox")

__all__ = ["BoxTunnel", "NetworkHandle", "SimpleBox"]


class StreamType(IntEnum):
    """Stream type for command execution output."""

    STDOUT = 1
    STDERR = 2


class BoxTunnel:
    """Prepared one-shot tunnel handle for a box service port."""

    def __init__(self, tunnel) -> None:
        self._tunnel = tunnel

    async def connect(self):
        """Consume the tunnel and return its bidirectional byte stream."""
        return await self._tunnel.connect()

    def uri(self) -> str | None:
        """Return the public URL of a remote tunnel, or ``None`` for a local one."""
        return self._tunnel.uri()

    async def forward(self, listen) -> "TunnelForwarder":
        return await self._tunnel.forward(listen)


class NetworkHandle:
    """Network operations for a ``SimpleBox``."""

    def __init__(self, box: "SimpleBox") -> None:
        self._owner = box

    async def tunnel(self, port: int) -> BoxTunnel:
        """Establish and return a one-shot tunnel for a port inside the box."""
        if not self._owner._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )
        return BoxTunnel(await self._owner._create_tunnel(port))


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
        image: str | None = None,
        rootfs_path: str | None = None,
        memory_mib: int | None = None,
        cpus: int | None = None,
        runtime: Optional["Boxlite"] = None,
        name: str | None = None,
        auto_remove: bool = True,
        reuse_existing: bool = False,
        **kwargs,
    ):
        """
        Create a specialized box.

        Args:
            image: Container image to use (e.g., "python:3.12-slim")
            rootfs_path: Path to local OCI layout directory (overrides image if provided)
            memory_mib: Memory limit in MiB
            cpus: Number of CPU cores
            runtime: Optional runtime instance (uses global default if None)
            name: Optional name for the box (must be unique)
            auto_remove: Remove box when stopped (default: True)
            reuse_existing: If True and a box with the given name already exists,
                reuse it instead of raising an error (default: False)
            **kwargs: Additional configuration options

        Note: The box is not actually created until entering the async context manager.
        Use `async with SimpleBox(...) as box:` to create and start the box.

        Either `image` or `rootfs_path` must be provided.
        """
        if not image and not rootfs_path:
            raise ValueError("Either 'image' or 'rootfs_path' must be provided")

        try:
            from .boxlite import Boxlite, BoxOptions
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

        # Store box options for deferred creation in __aenter__
        self._box_options = BoxOptions(
            image=image,
            rootfs_path=rootfs_path,
            cpus=cpus,
            memory_mib=memory_mib,
            auto_remove=auto_remove,
            **kwargs,
        )
        self._name = name
        self._reuse_existing = reuse_existing
        self._box = None
        self._started = False
        self._created: bool | None = None
        self._network = NetworkHandle(self)

    async def _create_tunnel(self, port: int):
        """Establish a native tunnel handle for a service port."""
        if self._box is None:
            raise RuntimeError("Box not created")
        if not isinstance(port, int) or not 1 <= port <= 65535:
            raise ValueError("port must be an integer between 1 and 65535")
        return await self._box.network.tunnel(port)

    async def __aenter__(self):
        """Async context manager entry - creates or reuses an existing box.

        This method is idempotent - calling it multiple times is safe.
        All initialization logic lives here; start() is just an alias.

        When a name is provided, attempts to get an existing box first.
        This enables persistence across sessions with auto_remove=False.
        """
        if self._started:
            return self
        if self._reuse_existing:
            self._box, self._created = await self._runtime.get_or_create(
                self._box_options, name=self._name
            )
        else:
            self._box = await self._runtime.create(self._box_options, name=self._name)
            self._created = True
        await self._box.__aenter__()
        self._started = True
        return self

    async def start(self):
        """
        Explicitly create and start the box.

        Alternative to using context manager. Allows::

            box = SimpleBox(image="alpine:latest")
            await box.start()
            await box.exec("echo", "hello")

        Returns:
            self for method chaining
        """
        return await self.__aenter__()

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        """Async context manager exit - delegates to Box.__aexit__ (returns awaitable)."""
        return await self._box.__aexit__(exc_type, exc_val, exc_tb)

    @property
    def id(self) -> str:
        """Get the box ID."""
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )
        return self._box.id

    async def info(self):
        """Get box information."""
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )
        return await self._box.info()

    @property
    def created(self) -> bool | None:
        """Whether this box was newly created (True) or an existing box was reused (False).

        Returns None if the box hasn't been started yet.
        """
        return self._created

    @property
    def network(self) -> NetworkHandle:
        """Get the box-scoped network handle."""
        if not hasattr(self, "_network"):
            self._network = NetworkHandle(self)
        return self._network

    async def exec(
        self,
        cmd: str,
        *args: str,
        env: dict[str, str] | None = None,
        user: str | None = None,
        timeout: float | None = None,
        cwd: str | None = None,
    ) -> ExecResult:
        """
        Execute a command in the box and return the result.

        Args:
            cmd: Command to execute (e.g., 'ls', 'python')
            *args: Arguments to the command (e.g., '-l', '-a')
            env: Environment variables (default: guest's default environment)
            user: User to run as (format: <name|uid>[:<group|gid>], like docker exec --user).
                  If None, uses the container's default user from image config.
            timeout: Execution timeout in seconds (default: no timeout).
            cwd: Working directory inside the container (default: container's configured workdir).

        Returns:
            ExecResult with exit_code and output

        Examples:
            Simple execution::

                result = await box.exec('ls', '-l', '-a')

            Run as a specific user::

                result = await box.exec('whoami', user='nobody')

            Run in a specific directory::

                result = await box.exec('pwd', cwd='/tmp')
        """
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )

        arg_list = list(args) if args else None
        # Convert env dict to list of tuples if provided
        env_list = list(env.items()) if env else None

        # Execute via Rust (returns PyExecution)
        execution = await self._box.exec(
            cmd, arg_list, env_list, user=user, timeout_secs=timeout, cwd=cwd
        )

        # Get streams from Rust execution
        try:
            stdout = execution.stdout()
        except Exception as e:  # noqa: BLE001 - native binding call; fall back to no stdout stream
            logger.error(f"take stdout err: {e}")
            stdout = None

        try:
            stderr = execution.stderr()
        except Exception as e:  # noqa: BLE001 - native binding call; fall back to no stderr stream
            logger.error(f"take stderr err: {e}")
            stderr = None

        # Collect stdout and stderr concurrently to avoid deadlock.
        # Sequential reads can deadlock when a process fills one pipe buffer
        # while the SDK is blocked reading the other.
        stdout_lines = []
        stderr_lines = []

        async def collect_stdout():
            if not stdout:
                return
            logger.debug("collecting stdout")
            try:
                async for line in stdout:
                    if isinstance(line, bytes):
                        stdout_lines.append(line.decode("utf-8", errors="replace"))
                    else:
                        stdout_lines.append(line)
            except Exception as e:  # noqa: BLE001 - stream collection must not crash exec(); partial output is acceptable
                logger.error(f"collecting stdout err: {e}")

        async def collect_stderr():
            if not stderr:
                return
            logger.debug("collecting stderr")
            try:
                async for line in stderr:
                    if isinstance(line, bytes):
                        stderr_lines.append(line.decode("utf-8", errors="replace"))
                    else:
                        stderr_lines.append(line)
            except Exception as e:  # noqa: BLE001 - stream collection must not crash exec(); partial output is acceptable
                logger.error(f"collecting stderr err: {e}")

        await asyncio.gather(collect_stdout(), collect_stderr())

        stdout = "".join(stdout_lines)
        stderr = "".join(stderr_lines)

        error_message = None
        try:
            exec_result = await execution.wait()
            exit_code = exec_result.exit_code
            error_message = exec_result.error_message
        except Exception as e:  # noqa: BLE001 - native binding call; report failure via exit_code instead of raising
            logger.error(f"failed to wait execution: {e}")
            exit_code = -1

        logger.debug(f"exec finish, exit_code: {exit_code}")

        return ExecResult(
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr,
            error_message=error_message,
        )

    async def tunnel(self, port: int) -> BoxTunnel:
        """Establish and return a one-shot tunnel for a port inside this box."""
        return await self.network.tunnel(port)

    async def metrics(self):
        """Get box metrics (CPU, memory usage)."""
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )
        return await self._box.metrics()

    async def stop(self):
        """
        Stop the box and release resources.

        Note: Usually not needed as context manager handles cleanup.
        """
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )
        await self._box.stop()
        self._started = False

    async def shutdown(self):
        """
        Shutdown the box and release resources.

        Alias for stop(). Usually not needed as context manager handles cleanup.
        """
        await self.stop()

    async def copy_in(
        self,
        host_path: str,
        container_dest: str,
        *,
        overwrite: bool = True,
        follow_symlinks: bool = False,
        include_parent: bool = True,
    ) -> None:
        """
        Copy files/directories from host into the container.

        Args:
            host_path: Path on the host filesystem (file or directory)
            container_dest: Destination path inside the container
            overwrite: If True, overwrite existing files (default: True)
            follow_symlinks: If True, follow symlinks when copying (default: False)
            include_parent: If True, include parent directory in archive (default: True)

        Note:
            copy_in extracts files into the container rootfs layer. Destinations
            under a mount inside the guest (e.g. /tmp, /dev/shm, volumes) are
            **refused** with an error naming the mount, because files written
            there would land behind it and be invisible to running processes.
            ``docker cp`` has the same blind spot but answers from the shadowed
            layer silently (see https://github.com/moby/moby/issues/22020).

            Files land owned by the box's exec user, so the workload can read
            them without any chmod/chown of its own.

            Workaround: use the low-level exec API to pipe a tar archive
            into the container (like ``docker exec -i CONTAINER tar xf -``)::

                execution = await box._box.exec("tar", args=["xf", "-", "-C", "/tmp"])
                stdin = execution.stdin()
                await stdin.send_input(tar_bytes)
                await stdin.close()
                result = await execution.wait()

        Examples:
            Copy a single file::

                await box.copy_in("/local/config.json", "/app/config.json")

            Copy a directory::

                await box.copy_in("/local/data/", "/app/data/")
        """
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )

        from .boxlite import CopyOptions

        opts = CopyOptions(
            recursive=True,
            overwrite=overwrite,
            follow_symlinks=follow_symlinks,
            include_parent=include_parent,
        )
        await self._box.copy_in(host_path, container_dest, opts)

    async def copy_out(
        self,
        container_src: str,
        host_dest: str,
        *,
        overwrite: bool = True,
        follow_symlinks: bool = False,
        include_parent: bool = True,
    ) -> None:
        """
        Copy files/directories from container to host.

        Args:
            container_src: Source path inside the container (file or directory)
            host_dest: Destination path on the host filesystem
            overwrite: If True, overwrite existing files (default: True)
            follow_symlinks: If True, follow symlinks when copying (default: False)
            include_parent: If True, include parent directory in archive (default: True)

        Note:
            copy_out reads the container rootfs layer. A source at or under a
            mount inside the guest (e.g. /tmp, /dev/shm, volumes), or a
            directory containing one, is **refused** with an error naming the
            mount, because the archive would carry the files underneath it
            rather than the ones running processes see.

            Workaround: use the low-level exec API to pipe a tar archive out
            of the container (``tar cf - -C /tmp .``).

        Examples:
            Copy a single file::

                await box.copy_out("/app/output.log", "/local/output.log")

            Copy a directory::

                await box.copy_out("/app/results/", "/local/results/")
        """
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )

        from .boxlite import CopyOptions

        opts = CopyOptions(
            recursive=True,
            overwrite=overwrite,
            follow_symlinks=follow_symlinks,
            include_parent=include_parent,
        )
        await self._box.copy_out(container_src, host_dest, opts)

    async def copy_in_stream(
        self,
        container_dest: str,
        source,
        *,
        source_is_dir: bool | None = None,
        overwrite: bool = True,
    ) -> None:
        """
        Stream a tar archive into the container, with no file on the host.

        For payloads that have no path: a tar built in memory, one being
        relayed from elsewhere, or a file too large to want a second copy of.
        The bytes are forwarded to the box as they arrive — nothing is
        buffered whole on either side.

        Args:
            container_dest: Destination path inside the container
            source: The archive bytes — ``bytes``, an iterable or async
                iterable of ``bytes``, or any object with a ``read(n)`` method
                (an open file, ``io.BytesIO``, a socket wrapper)
            source_is_dir: Shape of the archive: True for a directory tree,
                False for a single file. Leave as None only when the shape is
                genuinely unknown — the box then has to stage the archive to
                peek at it
            overwrite: If True, overwrite existing files (default: True)

        Raises:
            The copy's own error — a refused destination, a failed extraction —
            is raised at the end of the transfer, not from the first chunk. A
            source that fails part-way aborts the copy rather than committing
            the truncated archive.

        Examples:
            Stream a tar someone handed you::

                await box.copy_in_stream("/app", tar_bytes, source_is_dir=True)

            Stream a file without loading it::

                with open("big.tar", "rb") as f:
                    await box.copy_in_stream("/app", f, source_is_dir=True)
        """
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )

        from .boxlite import CopyOptions

        opts = CopyOptions(recursive=True, overwrite=overwrite)
        stream = self._box.copy_in_stream(container_dest, source_is_dir, opts)
        try:
            async for chunk in _as_chunks(source):
                await stream.write(chunk)
        except BaseException:
            # The archive is incomplete; a clean close would hand the box a
            # truncated tar, which extracts without complaint.
            await stream.abort()
            raise
        await stream.close()

    async def copy_out_stream(
        self,
        container_src: str,
        *,
        include_parent: bool = True,
        follow_symlinks: bool = False,
    ):
        """
        Stream a tar archive out of the container, with no file on the host.

        Args:
            container_src: Source path inside the container
            include_parent: If True, include the parent directory in the
                archive (default: True)
            follow_symlinks: If True, follow symlinks when archiving
                (default: False)

        Returns:
            An async-iterable of ``bytes`` chunks, carrying the archive's shape
            on its ``source_is_dir`` attribute (None when the box could not
            tell). Iteration raises if the transfer is cut short, so a
            truncated archive cannot pass for a whole one.

        Examples:
            ::

                stream = await box.copy_out_stream("/app/out")
                async for chunk in stream:
                    sink.write(chunk)
        """
        if not self._started:
            raise RuntimeError(
                "Box not started. Use 'async with SimpleBox(...) as box:' "
                "or call 'await box.start()' first."
            )

        from .boxlite import CopyOptions

        opts = CopyOptions(
            recursive=True,
            follow_symlinks=follow_symlinks,
            include_parent=include_parent,
        )
        return await self._box.copy_out_stream(container_src, opts)


#: Bytes read per ``read()`` call when ``copy_in_stream`` is handed a
#: file-like object. Matches the 1 MiB chunk the transfer itself uses, so a
#: file is read at the same granularity it is sent.
_COPY_CHUNK_BYTES = 1024 * 1024


async def _as_chunks(source):
    """
    Normalise every shape a caller may hand to ``copy_in_stream`` into an
    async iterator of ``bytes``.

    Accepts raw bytes, a file-like object (anything with ``read(n)``, sync or
    async), and sync or async iterables of bytes — the point being that
    ``open(path, "rb")`` and an ``async def`` generator both just work.
    """
    if isinstance(source, (bytes, bytearray, memoryview)):
        yield bytes(source)
        return

    read = getattr(source, "read", None)
    if callable(read):
        while True:
            chunk = read(_COPY_CHUNK_BYTES)
            if inspect.isawaitable(chunk):
                chunk = await chunk
            if not chunk:
                return
            yield bytes(chunk)

    if hasattr(source, "__aiter__"):
        async for chunk in source:
            yield bytes(chunk)
        return

    if hasattr(source, "__iter__"):
        for chunk in source:
            yield bytes(chunk)
        return

    raise TypeError(
        "copy_in_stream source must be bytes, a file-like object, or an "
        f"(async) iterable of bytes, not {type(source).__name__}"
    )
