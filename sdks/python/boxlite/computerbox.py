"""
ComputerBox - Desktop environment with web access.

Provides a minimal, elegant API for running isolated desktop environments
that can be viewed from a browser, with full GUI automation support.
"""

import asyncio
import base64
import logging
from typing import Optional, Tuple, TYPE_CHECKING

from .errors import ExecError, TimeoutError, ParseError
from .simplebox import SimpleBox

if TYPE_CHECKING:
    from .boxlite import Boxlite

__all__ = ["ComputerBox"]

# Configure logger
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

    # Always use xfce desktop
    _IMAGE_REFERENCE = "lscr.io/linuxserver/webtop:ubuntu-xfce"
    # Webtop uses port 3001 with HTTPS
    _GUEST_GUI_HTTP_PORT = 3000
    _GUEST_GUI_HTTPS_PORT = 3001
    # Webtop display number
    _DISPLAY_NUMBER = ":1"
    # Expected display resolution when SELKIES_IS_MANUAL_RESOLUTION_MODE=true (Anthropic requires ≤ 1280x800)
    # Webtop/Selkies defaults to 1024x768 in manual resolution mode
    _DEFAULT_DISPLAY_WIDTH_PX = 1024
    _DEFAULT_DISPLAY_HEIGHT_PX = 768

    def __init__(self, cpu: int = 2, memory: int = 2048, gui_http_port: int = 3000,
                 gui_https_port: int = 3001, runtime: Optional['Boxlite'] = None,
                 **kwargs):
        """
        Create and auto-start a desktop environment.

        Args:
            memory: Memory in MiB (default: 2048)
            cpu: Number of CPU cores (default: 2)
            gui_http_port: Port for web-based desktop GUI over HTTP (default: 3000)
            gui_https_port: Port for web-based desktop GUI over HTTPS (default: 3001)
            runtime: Optional runtime instance (uses global default if None)
            **kwargs: Additional configuration options (volumes, etc.)
        """
        # Merge user-provided env with default env
        user_env = kwargs.pop('env', [])
        default_env = [
            ("DISPLAY", self._DISPLAY_NUMBER),
            ("DISPLAY_SIZEW", str(self._DEFAULT_DISPLAY_WIDTH_PX)),
            ("DISPLAY_SIZEH", str(self._DEFAULT_DISPLAY_HEIGHT_PX)),
            ("SELKIES_MANUAL_WIDTH", str(self._DEFAULT_DISPLAY_WIDTH_PX)),
            ("SELKIES_MANUAL_HEIGHT", str(self._DEFAULT_DISPLAY_HEIGHT_PX)),
            ("SELKIES_UI_SHOW_SIDEBAR", "false"),
        ]
        merged_env = default_env + list(user_env)

        # Merge user-provided ports with default ports
        user_ports = kwargs.pop('ports', [])
        default_ports = [
            (gui_http_port, self._GUEST_GUI_HTTP_PORT),
            (gui_https_port, self._GUEST_GUI_HTTPS_PORT)
        ]
        merged_ports = default_ports + list(user_ports)

        super().__init__(
            image=self._IMAGE_REFERENCE,
            memory_mib=memory,
            cpus=cpu,
            runtime=runtime,
            env=merged_env,
            ports=merged_ports,
            **kwargs
        )

    async def wait_until_ready(self, timeout: int = 60):
        """
        Wait until the desktop environment is fully loaded and ready.

        Waits for xfdesktop to render the desktop, which ensures screenshots won't be black.

        Args:
            timeout: Maximum time to wait in seconds (default: 60)

        Raises:
            TimeoutError: If desktop doesn't become ready within timeout period

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.wait_until_ready()
            ...     # Desktop is now ready for automation and screenshots
        """
        logger.info("Waiting for desktop to become ready...")
        import time
        start_time = time.time()
        retry_delay = 0.5

        while True:
            elapsed = time.time() - start_time
            if elapsed > timeout:
                raise TimeoutError(f"Desktop did not become ready within {timeout} seconds")

            try:
                # Check if xfdesktop window exists at correct resolution
                exec_result = await self.exec("xwininfo", "-tree", "-root")
                result = exec_result.stdout
                expected_size = f'{self._DEFAULT_DISPLAY_WIDTH_PX}x{self._DEFAULT_DISPLAY_HEIGHT_PX}'

                logger.debug(f"stdout {result}")

                if 'xfdesktop' in result and expected_size in result:
                    logger.info(f"Desktop ready after {elapsed:.1f} seconds")
                    return

                logger.debug(f"Desktop not ready yet (waited {elapsed:.1f}s), retrying...")
                await asyncio.sleep(retry_delay)

            except (ExecError, ConnectionError, OSError, asyncio.TimeoutError) as e:
                # Transient errors - guest not ready yet, retry
                logger.debug(f"Desktop not ready: {e}, retrying...")
                await asyncio.sleep(retry_delay)
            except Exception as e:
                # Fatal errors - propagate immediately (e.g., spawn failure, permission denied)
                logger.error(f"Fatal error in wait_until_ready: {e}")
                raise

    # GUI Automation Methods

    async def screenshot(self) -> dict:
        """
        Capture a screenshot of the desktop using PIL.ImageGrab (pre-installed).

        Note: Screenshots may be black if taken before the XFCE desktop has fully
        initialized. Use wait_until_ready() before taking screenshots to ensure
        the desktop has been rendered.

        Returns:
            Dictionary containing:
            - data: Base64-encoded PNG images data
            - width: Display width in pixels (1024)
            - height: Display height in pixels (768)
            - format: Image format ("png")

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.wait_until_ready()  # Ensure desktop is rendered
            ...     result = await desktop.screenshot()
            ...     image_data = base64.b64decode(result['data'])
            ...     with open('screenshot.png', 'wb') as f:
            ...         f.write(image_data)
        """
        logger.info("Taking screenshot...")

        # Use PIL.ImageGrab (pre-installed in webtop) to capture screenshot
        # This avoids needing to install scrot and is faster
        logger.debug("Capturing screenshot with PIL.ImageGrab...")
        python_code = '''
from PIL import ImageGrab
import io
import base64

# Capture screenshot
img = ImageGrab.grab()

# Convert to PNG in memory
buffer = io.BytesIO()
img.save(buffer, format="PNG")

# Output base64-encoded PNG
print(base64.b64encode(buffer.getvalue()).decode("utf-8"))
'''
        # Execute and get stdout
        exec_result = await self.exec("python3", "-c", python_code)

        # Check if screenshot command succeeded
        if exec_result.exit_code != 0:
            logger.error(f"Screenshot failed with exit code {exec_result.exit_code}")
            logger.error(f"stderr: {exec_result.stderr}")
            raise ExecError("screenshot()", exec_result.exit_code, exec_result.stderr)

        b64_data = exec_result.stdout.strip()

        logger.info(
            f"Screenshot captured: {self._DEFAULT_DISPLAY_WIDTH_PX}x{self._DEFAULT_DISPLAY_HEIGHT_PX}")
        return {
            "data": b64_data,
            "width": self._DEFAULT_DISPLAY_WIDTH_PX,
            "height": self._DEFAULT_DISPLAY_HEIGHT_PX,
            "format": "png"
        }

    async def mouse_move(self, x: int, y: int):
        """
        Move mouse cursor to absolute coordinates.

        Args:
            x: X coordinate
            y: Y coordinate

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.mouse_move(100, 200)
        """
        logger.info(f"Moving mouse to ({x}, {y})")
        exec_result = await self.exec("xdotool", "mousemove", str(x), str(y))
        if exec_result.exit_code != 0:
            raise ExecError(f"mouse_move({x}, {y})", exec_result.exit_code, exec_result.stderr)
        logger.debug(f"Mouse moved to ({x}, {y})")

    async def left_click(self):
        """
        Click left mouse button at current position.

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.mouse_move(100, 200)
            ...     await desktop.left_click()
        """
        logger.info("Clicking left mouse button")
        exec_result = await self.exec("xdotool", "click", "1")
        if exec_result.exit_code != 0:
            raise ExecError("left_click()", exec_result.exit_code, exec_result.stderr)
        logger.debug("Clicked left button")

    async def right_click(self):
        """
        Click right mouse button at current position.

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.mouse_move(100, 200)
            ...     await desktop.right_click()
        """
        logger.info("Clicking right mouse button")
        exec_result = await self.exec("xdotool", "click", "3")
        if exec_result.exit_code != 0:
            raise ExecError("right_click()", exec_result.exit_code, exec_result.stderr)
        logger.debug("Clicked right button")

    async def middle_click(self):
        """
        Click middle mouse button at current position.

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.mouse_move(100, 200)
            ...     await desktop.middle_click()
        """
        logger.info("Clicking middle mouse button")
        exec_result = await self.exec("xdotool", "click", "2")
        if exec_result.exit_code != 0:
            raise ExecError("middle_click()", exec_result.exit_code, exec_result.stderr)
        logger.debug("Clicked middle button")

    async def double_click(self):
        """
        Double-click left mouse button at current position.

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.mouse_move(100, 200)
            ...     await desktop.double_click()
        """
        logger.info("Double-clicking left mouse button")
        exec_result = await self.exec("xdotool", "click", "--repeat", "2", "--delay",
                                      "100", "1")
        if exec_result.exit_code != 0:
            raise ExecError("double_click()", exec_result.exit_code, exec_result.stderr)
        logger.debug("Double-clicked left button")

    async def triple_click(self):
        """
        Triple-click left mouse button at current position.

        Useful for selecting entire lines or paragraphs of text.

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.mouse_move(100, 200)
            ...     await desktop.triple_click()
        """
        logger.info("Triple-clicking left mouse button")
        # Anthropic requires 100-200ms delays between clicks
        exec_result = await self.exec("xdotool", "click", "--repeat", "3", "--delay",
                                      "100", "1")
        if exec_result.exit_code != 0:
            raise ExecError("triple_click()", exec_result.exit_code, exec_result.stderr)
        logger.debug("Triple-clicked left button")

    async def left_click_drag(self, start_x: int, start_y: int, end_x: int, end_y: int):
        """
        Drag mouse from start position to end position with left button held.

        Args:
            start_x: Starting X coordinate
            start_y: Starting Y coordinate
            end_x: Ending X coordinate
            end_y: Ending Y coordinate

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.left_click_drag(100, 100, 200, 200)
        """
        logger.info(f"Dragging from ({start_x}, {start_y}) to ({end_x}, {end_y})")
        # Chain all operations in single xdotool command: move, press, move, release
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
        logger.debug(f"Drag completed")

    async def cursor_position(self) -> Tuple[int, int]:
        """
        Get the current mouse cursor position.

        Returns:
            Tuple of (x, y) coordinates

        Example:
            >>> async with ComputerBox() as desktop:
            ...     x, y = await desktop.cursor_position()
            ...     print(f"Cursor at ({x}, {y})")
        """
        logger.info("Getting cursor position")

        # Use xdotool to get mouse location
        exec_result = await self.exec("xdotool", "getmouselocation", "--shell")

        # Check if command succeeded
        if exec_result.exit_code != 0:
            logger.error(f"xdotool failed with exit code {exec_result.exit_code}")
            logger.error(f"stderr: {exec_result.stderr}")
            raise ExecError("cursor_position()", exec_result.exit_code, exec_result.stderr)

        # Parse output (format: "X=123\nY=456\nSCREEN=0\nWINDOW=...")
        x, y = None, None
        for line in exec_result.stdout.split('\n'):
            clean_line = line.strip()
            if clean_line.startswith('X='):
                x = int(clean_line[2:])
            elif clean_line.startswith('Y='):
                y = int(clean_line[2:])

        if x is not None and y is not None:
            logger.info(f"Cursor position: ({x}, {y})")
            return (x, y)

        logger.error("Failed to parse cursor position from xdotool output")
        raise ParseError("Failed to parse cursor position from xdotool output")

    async def type(self, text: str):
        """
        Type text using the keyboard.

        Args:
            text: Text to type

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.type("Hello World!")
        """
        logger.info(f"Typing text: {text[:50]}{'...' if len(text) > 50 else ''}")

        # Escape special characters for xdotool
        exec_result = await self.exec("xdotool", "type", "--", text)
        if exec_result.exit_code != 0:
            raise ExecError("type()", exec_result.exit_code, exec_result.stderr)
        logger.debug(f"Typed {len(text)} characters")

    async def key(self, text: str):
        """
        Press a special key or key combination.

        Args:
            text: Key to press (e.g., 'Return', 'Escape', 'ctrl+c', 'alt+F4')

        Special keys: Return, Escape, Tab, space, BackSpace, Delete,
                     Up, Down, Left, Right, Home, End, Page_Up, Page_Down,
                     F1-F12, etc.

        Example:
            >>> async with ComputerBox() as desktop:
            ...     await desktop.key("Return")
            ...     await desktop.key("ctrl+c")
        """
        logger.info(f"Pressing key: {text}")
        exec_result = await self.exec("xdotool", "key", text)
        if exec_result.exit_code != 0:
            raise ExecError("key()", exec_result.exit_code, exec_result.stderr)
        logger.debug(f"Pressed key: {text}")

    async def scroll(self, x: int, y: int, direction: str, amount: int = 3):
        """
        Scroll at a specific position.

        Args:
            x: X coordinate where to scroll
            y: Y coordinate where to scroll
            direction: Scroll direction - 'up', 'down', 'left', or 'right'
            amount: Number of scroll units (default: 3)

        Example:
            >>> async with ComputerBox() as desktop:
            ...     # Scroll up in the middle of the screen
            ...     await desktop.scroll(512, 384, "up", amount=5)
        """
        logger.info(f"Scrolling {direction} at ({x}, {y}), amount={amount}")

        # Map scroll directions to xdotool mouse button numbers
        # In X11, scroll is simulated using mouse button clicks:
        # Button 4 = scroll up, Button 5 = scroll down
        # Button 6 = scroll left, Button 7 = scroll right
        direction_map = {
            "up": "4",
            "down": "5",
            "left": "6",
            "right": "7"
        }

        button = direction_map.get(direction.lower())
        if not button:
            raise ValueError(
                f"Invalid scroll direction: {direction}. Must be 'up', 'down', 'left', or 'right'")

        # Chain mousemove and repeated clicks in single xdotool command
        exec_result = await self.exec(
            "xdotool",
            "mousemove", str(x), str(y),
            "click", "--repeat", str(amount), button
        )

        # Check if command succeeded
        if exec_result.exit_code != 0:
            logger.error(f"xdotool scroll failed with exit code {exec_result.exit_code}")
            logger.error(f"stderr: {exec_result.stderr}")
            raise ExecError("scroll()", exec_result.exit_code, exec_result.stderr)

        logger.debug(f"Scrolled {direction} {amount} times at ({x}, {y})")

    async def get_screen_size(self) -> Tuple[int, int]:
        """
        Get the screen resolution.

        Returns:
            Tuple of (width, height)

        Example:
            >>> async with ComputerBox() as desktop:
            ...     width, height = await desktop.get_screen_size()
            ...     print(f"Screen: {width}x{height}")
        """
        logger.info("Getting screen size")

        # Use xdotool to get screen size
        exec_result = await self.exec("xdotool", "getdisplaygeometry")

        # Check if command succeeded (exit code is more reliable than stderr presence)
        if exec_result.exit_code != 0:
            logger.error(f"xdotool failed with exit code {exec_result.exit_code}")
            logger.error(f"stderr: {exec_result.stderr}")
            # Raise exception with stderr content so wait_until_ready() can detect it
            raise ExecError("get_screen_size()", exec_result.exit_code, exec_result.stderr)

        # Parse stdout (format: "width height")
        result = exec_result.stdout.strip()
        logger.debug(f"stdout result: {result}")
        parts = result.split()
        if len(parts) == 2:
            size = (int(parts[0]), int(parts[1]))
            logger.info(f"Screen size: {size[0]}x{size[1]}")
            return size

        logger.error("Failed to parse screen size from xdotool output")
        raise ParseError("Failed to parse screen size from xdotool output")
