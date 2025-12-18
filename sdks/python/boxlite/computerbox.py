"""
ComputerBox - Desktop environment with web access.

Provides a minimal, elegant API for running isolated desktop environments
that can be viewed from a browser, with full GUI automation support.
"""

import asyncio
import logging
from typing import Optional, Tuple, TYPE_CHECKING

from . import constants as const
from .errors import ExecError, TimeoutError, ParseError
from .simplebox import SimpleBox

if TYPE_CHECKING:
    from .boxlite import Boxlite

__all__ = ["ComputerBox"]

logger = logging.getLogger("boxlite.computerbox")


class ComputerBox(SimpleBox):
    """
    Desktop environment accessible via web browser.

    Auto-starts a full desktop environment with web interface.
    Access the desktop by opening the URL in your browser.

    Note: Uses HTTPS with self-signed certificate - your browser will show
    a security warning. Click "Advanced" and "Proceed" to access the desktop.

    Usage:
        >>> async with ComputerBox() as desktop:
        ...     await desktop.wait_until_ready()
        ...     screenshot = await desktop.screenshot()

    Example with custom settings:
        >>> async with ComputerBox(memory=4096, cpu=4) as desktop:
        ...     await desktop.mouse_move(100, 200)
        ...     await desktop.left_click()
    """

    def __init__(
        self,
        cpu: int = const.COMPUTERBOX_CPUS,
        memory: int = const.COMPUTERBOX_MEMORY_MIB,
        gui_http_port: int = const.COMPUTERBOX_GUI_HTTP_PORT,
        gui_https_port: int = const.COMPUTERBOX_GUI_HTTPS_PORT,
        runtime: Optional['Boxlite'] = None,
        **kwargs
    ):
        """
        Create and auto-start a desktop environment.

        Args:
            cpu: Number of CPU cores (default: 2)
            memory: Memory in MiB (default: 2048)
            gui_http_port: Port for HTTP desktop GUI (default: 3000)
            gui_https_port: Port for HTTPS desktop GUI (default: 3001)
            runtime: Optional runtime instance (uses global default if None)
            **kwargs: Additional configuration options (volumes, etc.)
        """
        user_env = kwargs.pop('env', [])
        default_env = [
            ("DISPLAY", const.COMPUTERBOX_DISPLAY_NUMBER),
            ("DISPLAY_SIZEW", str(const.COMPUTERBOX_DISPLAY_WIDTH)),
            ("DISPLAY_SIZEH", str(const.COMPUTERBOX_DISPLAY_HEIGHT)),
            ("SELKIES_MANUAL_WIDTH", str(const.COMPUTERBOX_DISPLAY_WIDTH)),
            ("SELKIES_MANUAL_HEIGHT", str(const.COMPUTERBOX_DISPLAY_HEIGHT)),
            ("SELKIES_UI_SHOW_SIDEBAR", "false"),
        ]

        user_ports = kwargs.pop('ports', [])
        default_ports = [
            (gui_http_port, const.COMPUTERBOX_GUI_HTTP_PORT),
            (gui_https_port, const.COMPUTERBOX_GUI_HTTPS_PORT)
        ]

        super().__init__(
            image=const.COMPUTERBOX_IMAGE,
            memory_mib=memory,
            cpus=cpu,
            runtime=runtime,
            env=default_env + list(user_env),
            ports=default_ports + list(user_ports),
            **kwargs
        )

    async def wait_until_ready(self, timeout: int = const.DESKTOP_READY_TIMEOUT):
        """
        Wait until the desktop environment is fully loaded and ready.

        Args:
            timeout: Maximum time to wait in seconds (default: 60)

        Raises:
            TimeoutError: If desktop doesn't become ready within timeout period
        """
        logger.info("Waiting for desktop to become ready...")
        import time
        start_time = time.time()

        while True:
            elapsed = time.time() - start_time
            if elapsed > timeout:
                raise TimeoutError(f"Desktop did not become ready within {timeout} seconds")

            try:
                exec_result = await self.exec("xwininfo", "-tree", "-root")
                expected_size = f'{const.COMPUTERBOX_DISPLAY_WIDTH}x{const.COMPUTERBOX_DISPLAY_HEIGHT}'

                if 'xfdesktop' in exec_result.stdout and expected_size in exec_result.stdout:
                    logger.info(f"Desktop ready after {elapsed:.1f} seconds")
                    return

                logger.debug(f"Desktop not ready yet (waited {elapsed:.1f}s), retrying...")
                await asyncio.sleep(const.DESKTOP_READY_RETRY_DELAY)

            except (ExecError, ConnectionError, OSError, asyncio.TimeoutError) as e:
                logger.debug(f"Desktop not ready: {e}, retrying...")
                await asyncio.sleep(const.DESKTOP_READY_RETRY_DELAY)
            except Exception as e:
                logger.error(f"Fatal error in wait_until_ready: {e}")
                raise

    async def screenshot(self) -> dict:
        """
        Capture a screenshot of the desktop.

        Returns:
            Dictionary with: data (base64 PNG), width, height, format
        """
        logger.info("Taking screenshot...")

        python_code = '''
from PIL import ImageGrab
import io
import base64
img = ImageGrab.grab()
buffer = io.BytesIO()
img.save(buffer, format="PNG")
print(base64.b64encode(buffer.getvalue()).decode("utf-8"))
'''
        exec_result = await self.exec("python3", "-c", python_code)

        if exec_result.exit_code != 0:
            raise ExecError("screenshot()", exec_result.exit_code, exec_result.stderr)

        return {
            "data": exec_result.stdout.strip(),
            "width": const.COMPUTERBOX_DISPLAY_WIDTH,
            "height": const.COMPUTERBOX_DISPLAY_HEIGHT,
            "format": "png"
        }

    async def mouse_move(self, x: int, y: int):
        """Move mouse cursor to absolute coordinates."""
        exec_result = await self.exec("xdotool", "mousemove", str(x), str(y))
        if exec_result.exit_code != 0:
            raise ExecError(f"mouse_move({x}, {y})", exec_result.exit_code, exec_result.stderr)

    async def left_click(self):
        """Click left mouse button at current position."""
        exec_result = await self.exec("xdotool", "click", "1")
        if exec_result.exit_code != 0:
            raise ExecError("left_click()", exec_result.exit_code, exec_result.stderr)

    async def right_click(self):
        """Click right mouse button at current position."""
        exec_result = await self.exec("xdotool", "click", "3")
        if exec_result.exit_code != 0:
            raise ExecError("right_click()", exec_result.exit_code, exec_result.stderr)

    async def middle_click(self):
        """Click middle mouse button at current position."""
        exec_result = await self.exec("xdotool", "click", "2")
        if exec_result.exit_code != 0:
            raise ExecError("middle_click()", exec_result.exit_code, exec_result.stderr)

    async def double_click(self):
        """Double-click left mouse button at current position."""
        exec_result = await self.exec("xdotool", "click", "--repeat", "2", "--delay", "100", "1")
        if exec_result.exit_code != 0:
            raise ExecError("double_click()", exec_result.exit_code, exec_result.stderr)

    async def triple_click(self):
        """Triple-click left mouse button at current position."""
        exec_result = await self.exec("xdotool", "click", "--repeat", "3", "--delay", "100", "1")
        if exec_result.exit_code != 0:
            raise ExecError("triple_click()", exec_result.exit_code, exec_result.stderr)

    async def left_click_drag(self, start_x: int, start_y: int, end_x: int, end_y: int):
        """Drag mouse from start position to end position with left button held."""
        exec_result = await self.exec(
            "xdotool",
            "mousemove", str(start_x), str(start_y),
            "mousedown", "1",
            "sleep", "0.1",
            "mousemove", str(end_x), str(end_y),
            "sleep", "0.1",
            "mouseup", "1"
        )
        if exec_result.exit_code != 0:
            raise ExecError("left_click_drag()", exec_result.exit_code, exec_result.stderr)

    async def cursor_position(self) -> Tuple[int, int]:
        """Get the current mouse cursor position. Returns (x, y) tuple."""
        exec_result = await self.exec("xdotool", "getmouselocation", "--shell")
        if exec_result.exit_code != 0:
            raise ExecError("cursor_position()", exec_result.exit_code, exec_result.stderr)

        x, y = None, None
        for line in exec_result.stdout.split('\n'):
            line = line.strip()
            if line.startswith('X='):
                x = int(line[2:])
            elif line.startswith('Y='):
                y = int(line[2:])

        if x is not None and y is not None:
            return (x, y)
        raise ParseError("Failed to parse cursor position from xdotool output")

    async def type(self, text: str):
        """Type text using the keyboard."""
        exec_result = await self.exec("xdotool", "type", "--", text)
        if exec_result.exit_code != 0:
            raise ExecError("type()", exec_result.exit_code, exec_result.stderr)

    async def key(self, text: str):
        """Press a special key or key combination (e.g., 'Return', 'ctrl+c')."""
        exec_result = await self.exec("xdotool", "key", text)
        if exec_result.exit_code != 0:
            raise ExecError("key()", exec_result.exit_code, exec_result.stderr)

    async def scroll(self, x: int, y: int, direction: str, amount: int = 3):
        """
        Scroll at a specific position.

        Args:
            x, y: Coordinates where to scroll
            direction: 'up', 'down', 'left', or 'right'
            amount: Number of scroll units (default: 3)
        """
        direction_map = {"up": "4", "down": "5", "left": "6", "right": "7"}
        button = direction_map.get(direction.lower())
        if not button:
            raise ValueError(f"Invalid scroll direction: {direction}")

        exec_result = await self.exec(
            "xdotool", "mousemove", str(x), str(y), "click", "--repeat", str(amount), button
        )
        if exec_result.exit_code != 0:
            raise ExecError("scroll()", exec_result.exit_code, exec_result.stderr)

    async def get_screen_size(self) -> Tuple[int, int]:
        """Get the screen resolution. Returns (width, height) tuple."""
        exec_result = await self.exec("xdotool", "getdisplaygeometry")
        if exec_result.exit_code != 0:
            raise ExecError("get_screen_size()", exec_result.exit_code, exec_result.stderr)

        parts = exec_result.stdout.strip().split()
        if len(parts) == 2:
            return (int(parts[0]), int(parts[1]))
        raise ParseError("Failed to parse screen size from xdotool output")
