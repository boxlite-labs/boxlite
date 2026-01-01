/**
 * ComputerBox - Desktop environment with web access.
 *
 * Provides a minimal, elegant API for running isolated desktop environments
 * that can be viewed from a browser, with full GUI automation support.
 */

import { SimpleBox, type SimpleBoxOptions } from './simplebox';
import { ExecError, TimeoutError, ParseError } from './errors';
import * as constants from './constants';

/**
 * Options for creating a ComputerBox.
 */
export interface ComputerBoxOptions extends Omit<SimpleBoxOptions, 'image' | 'cpus' | 'memoryMib'> {
  /** Number of CPU cores (default: 2) */
  cpus?: number;

  /** Memory in MiB (default: 2048) */
  memoryMib?: number;

  /** Port for HTTP desktop GUI (default: 3000) */
  guiHttpPort?: number;

  /** Port for HTTPS desktop GUI (default: 3001) */
  guiHttpsPort?: number;
}

/**
 * Screenshot result containing image data and metadata.
 */
export interface Screenshot {
  /** Base64-encoded PNG image data */
  data: string;

  /** Image width in pixels */
  width: number;

  /** Image height in pixels */
  height: number;

  /** Image format (always 'png') */
  format: 'png';
}

/**
 * Desktop environment accessible via web browser.
 *
 * Auto-starts a full desktop environment with web interface.
 * Access the desktop by opening the URL in your browser.
 *
 * **Note**: Uses HTTPS with self-signed certificate - your browser will show
 * a security warning. Click "Advanced" and "Proceed" to access the desktop.
 *
 * ## Usage
 *
 * ```typescript
 * const desktop = new ComputerBox();
 * try {
 *   await desktop.waitUntilReady();
 *   const screenshot = await desktop.screenshot();
 *   console.log('Desktop ready!');
 * } finally {
 *   await desktop.stop();
 * }
 * ```
 *
 * ## Example with custom settings
 *
 * ```typescript
 * const desktop = new ComputerBox({
 *   memoryMib: 4096,
 *   cpus: 4
 * });
 * try {
 *   await desktop.waitUntilReady();
 *   await desktop.mouseMove(100, 200);
 *   await desktop.leftClick();
 * } finally {
 *   await desktop.stop();
 * }
 * ```
 */
export class ComputerBox extends SimpleBox {
  /**
   * Create and auto-start a desktop environment.
   *
   * @param options - ComputerBox configuration options
   *
   * @example
   * ```typescript
   * const desktop = new ComputerBox({
   *   cpus: 2,
   *   memoryMib: 2048,
   *   guiHttpPort: 3000,
   *   guiHttpsPort: 3001
   * });
   * ```
   */
  constructor(options: ComputerBoxOptions = {}) {
    const {
      cpus = constants.COMPUTERBOX_CPUS,
      memoryMib = constants.COMPUTERBOX_MEMORY_MIB,
      guiHttpPort = constants.COMPUTERBOX_GUI_HTTP_PORT,
      guiHttpsPort = constants.COMPUTERBOX_GUI_HTTPS_PORT,
      env = {},
      ports = [],
      ...restOptions
    } = options;

    // Merge default and user environment variables
    const defaultEnv: Record<string, string> = {
      DISPLAY: constants.COMPUTERBOX_DISPLAY_NUMBER,
      DISPLAY_SIZEW: constants.COMPUTERBOX_DISPLAY_WIDTH.toString(),
      DISPLAY_SIZEH: constants.COMPUTERBOX_DISPLAY_HEIGHT.toString(),
      SELKIES_MANUAL_WIDTH: constants.COMPUTERBOX_DISPLAY_WIDTH.toString(),
      SELKIES_MANUAL_HEIGHT: constants.COMPUTERBOX_DISPLAY_HEIGHT.toString(),
      SELKIES_UI_SHOW_SIDEBAR: 'false',
    };

    // Merge default and user ports
    const defaultPorts = [
      { hostPort: guiHttpPort, guestPort: constants.COMPUTERBOX_GUI_HTTP_PORT },
      { hostPort: guiHttpsPort, guestPort: constants.COMPUTERBOX_GUI_HTTPS_PORT },
    ];

    super({
      ...restOptions,
      image: constants.COMPUTERBOX_IMAGE,
      cpus,
      memoryMib,
      env: { ...defaultEnv, ...env },
      ports: [...defaultPorts, ...ports],
    });
  }

  /**
   * Wait until the desktop environment is fully loaded and ready.
   *
   * @param timeout - Maximum time to wait in seconds (default: 60)
   *
   * @throws {TimeoutError} If desktop doesn't become ready within timeout period
   *
   * @example
   * ```typescript
   * const desktop = new ComputerBox();
   * try {
   *   await desktop.waitUntilReady(60);
   *   console.log('Desktop is ready!');
   * } finally {
   *   await desktop.stop();
   * }
   * ```
   */
  async waitUntilReady(timeout: number = constants.DESKTOP_READY_TIMEOUT): Promise<void> {
    const startTime = Date.now();

    while (true) {
      const elapsed = (Date.now() - startTime) / 1000;
      if (elapsed > timeout) {
        throw new TimeoutError(`Desktop did not become ready within ${timeout} seconds`);
      }

      try {
        const result = await this.exec('xwininfo', '-tree', '-root');
        const expectedSize = `${constants.COMPUTERBOX_DISPLAY_WIDTH}x${constants.COMPUTERBOX_DISPLAY_HEIGHT}`;

        if (result.stdout.includes('xfdesktop') && result.stdout.includes(expectedSize)) {
          return;
        }

        // Wait before retrying
        await new Promise(resolve => setTimeout(resolve, constants.DESKTOP_READY_RETRY_DELAY * 1000));
      } catch (error) {
        // Desktop not ready yet, retry
        await new Promise(resolve => setTimeout(resolve, constants.DESKTOP_READY_RETRY_DELAY * 1000));
      }
    }
  }

  /**
   * Capture a screenshot of the desktop.
   *
   * @returns Promise resolving to screenshot data with base64 PNG, dimensions, and format
   *
   * @example
   * ```typescript
   * const desktop = new ComputerBox();
   * try {
   *   await desktop.waitUntilReady();
   *   const screenshot = await desktop.screenshot();
   *   console.log(`Screenshot: ${screenshot.width}x${screenshot.height}`);
   *   // Save screenshot.data (base64 PNG) to file or process it
   * } finally {
   *   await desktop.stop();
   * }
   * ```
   */
  async screenshot(): Promise<Screenshot> {
    const pythonCode = `
from PIL import ImageGrab
import io
import base64
img = ImageGrab.grab()
buffer = io.BytesIO()
img.save(buffer, format="PNG")
print(base64.b64encode(buffer.getvalue()).decode("utf-8"))
`.trim();

    const result = await this.exec('python3', '-c', pythonCode);

    if (result.exitCode !== 0) {
      throw new ExecError('screenshot()', result.exitCode, result.stderr);
    }

    return {
      data: result.stdout.trim(),
      width: constants.COMPUTERBOX_DISPLAY_WIDTH,
      height: constants.COMPUTERBOX_DISPLAY_HEIGHT,
      format: 'png',
    };
  }

  /**
   * Move mouse cursor to absolute coordinates.
   *
   * @param x - X coordinate
   * @param y - Y coordinate
   *
   * @example
   * ```typescript
   * await desktop.mouseMove(100, 200);
   * ```
   */
  async mouseMove(x: number, y: number): Promise<void> {
    const result = await this.exec('xdotool', 'mousemove', x.toString(), y.toString());
    if (result.exitCode !== 0) {
      throw new ExecError(`mouseMove(${x}, ${y})`, result.exitCode, result.stderr);
    }
  }

  /**
   * Click left mouse button at current position.
   *
   * @example
   * ```typescript
   * await desktop.leftClick();
   * ```
   */
  async leftClick(): Promise<void> {
    const result = await this.exec('xdotool', 'click', '1');
    if (result.exitCode !== 0) {
      throw new ExecError('leftClick()', result.exitCode, result.stderr);
    }
  }

  /**
   * Click right mouse button at current position.
   *
   * @example
   * ```typescript
   * await desktop.rightClick();
   * ```
   */
  async rightClick(): Promise<void> {
    const result = await this.exec('xdotool', 'click', '3');
    if (result.exitCode !== 0) {
      throw new ExecError('rightClick()', result.exitCode, result.stderr);
    }
  }

  /**
   * Click middle mouse button at current position.
   *
   * @example
   * ```typescript
   * await desktop.middleClick();
   * ```
   */
  async middleClick(): Promise<void> {
    const result = await this.exec('xdotool', 'click', '2');
    if (result.exitCode !== 0) {
      throw new ExecError('middleClick()', result.exitCode, result.stderr);
    }
  }

  /**
   * Double-click left mouse button at current position.
   *
   * @example
   * ```typescript
   * await desktop.doubleClick();
   * ```
   */
  async doubleClick(): Promise<void> {
    const result = await this.exec('xdotool', 'click', '--repeat', '2', '--delay', '100', '1');
    if (result.exitCode !== 0) {
      throw new ExecError('doubleClick()', result.exitCode, result.stderr);
    }
  }

  /**
   * Triple-click left mouse button at current position.
   *
   * @example
   * ```typescript
   * await desktop.tripleClick();
   * ```
   */
  async tripleClick(): Promise<void> {
    const result = await this.exec('xdotool', 'click', '--repeat', '3', '--delay', '100', '1');
    if (result.exitCode !== 0) {
      throw new ExecError('tripleClick()', result.exitCode, result.stderr);
    }
  }

  /**
   * Drag mouse from start position to end position with left button held.
   *
   * @param startX - Starting X coordinate
   * @param startY - Starting Y coordinate
   * @param endX - Ending X coordinate
   * @param endY - Ending Y coordinate
   *
   * @example
   * ```typescript
   * await desktop.leftClickDrag(100, 100, 200, 200);
   * ```
   */
  async leftClickDrag(startX: number, startY: number, endX: number, endY: number): Promise<void> {
    const result = await this.exec(
      'xdotool',
      'mousemove', startX.toString(), startY.toString(),
      'mousedown', '1',
      'sleep', '0.1',
      'mousemove', endX.toString(), endY.toString(),
      'sleep', '0.1',
      'mouseup', '1'
    );
    if (result.exitCode !== 0) {
      throw new ExecError('leftClickDrag()', result.exitCode, result.stderr);
    }
  }

  /**
   * Get the current mouse cursor position.
   *
   * @returns Promise resolving to [x, y] coordinates
   *
   * @example
   * ```typescript
   * const [x, y] = await desktop.cursorPosition();
   * console.log(`Cursor at: ${x}, ${y}`);
   * ```
   */
  async cursorPosition(): Promise<[number, number]> {
    const result = await this.exec('xdotool', 'getmouselocation', '--shell');
    if (result.exitCode !== 0) {
      throw new ExecError('cursorPosition()', result.exitCode, result.stderr);
    }

    let x: number | undefined;
    let y: number | undefined;

    for (const line of result.stdout.split('\n')) {
      const trimmed = line.trim();
      if (trimmed.startsWith('X=')) {
        x = parseInt(trimmed.slice(2), 10);
      } else if (trimmed.startsWith('Y=')) {
        y = parseInt(trimmed.slice(2), 10);
      }
    }

    if (x !== undefined && y !== undefined) {
      return [x, y];
    }

    throw new ParseError('Failed to parse cursor position from xdotool output');
  }

  /**
   * Type text using the keyboard.
   *
   * @param text - Text to type
   *
   * @example
   * ```typescript
   * await desktop.type('Hello, World!');
   * ```
   */
  async type(text: string): Promise<void> {
    const result = await this.exec('xdotool', 'type', '--', text);
    if (result.exitCode !== 0) {
      throw new ExecError('type()', result.exitCode, result.stderr);
    }
  }

  /**
   * Press a special key or key combination.
   *
   * @param keySequence - Key or key combination (e.g., 'Return', 'ctrl+c', 'alt+Tab')
   *
   * @example
   * ```typescript
   * await desktop.key('Return');
   * await desktop.key('ctrl+c');
   * await desktop.key('alt+Tab');
   * ```
   */
  async key(keySequence: string): Promise<void> {
    const result = await this.exec('xdotool', 'key', keySequence);
    if (result.exitCode !== 0) {
      throw new ExecError('key()', result.exitCode, result.stderr);
    }
  }

  /**
   * Scroll at a specific position.
   *
   * @param x - X coordinate where to scroll
   * @param y - Y coordinate where to scroll
   * @param direction - Scroll direction: 'up', 'down', 'left', or 'right'
   * @param amount - Number of scroll units (default: 3)
   *
   * @example
   * ```typescript
   * await desktop.scroll(500, 300, 'down', 5);
   * ```
   */
  async scroll(x: number, y: number, direction: 'up' | 'down' | 'left' | 'right', amount: number = 3): Promise<void> {
    const directionMap: Record<string, string> = {
      up: '4',
      down: '5',
      left: '6',
      right: '7',
    };

    const button = directionMap[direction.toLowerCase()];
    if (!button) {
      throw new Error(`Invalid scroll direction: ${direction}`);
    }

    const result = await this.exec(
      'xdotool', 'mousemove', x.toString(), y.toString(), 'click', '--repeat', amount.toString(), button
    );
    if (result.exitCode !== 0) {
      throw new ExecError('scroll()', result.exitCode, result.stderr);
    }
  }

  /**
   * Get the screen resolution.
   *
   * @returns Promise resolving to [width, height] in pixels
   *
   * @example
   * ```typescript
   * const [width, height] = await desktop.getScreenSize();
   * console.log(`Screen: ${width}x${height}`);
   * ```
   */
  async getScreenSize(): Promise<[number, number]> {
    const result = await this.exec('xdotool', 'getdisplaygeometry');
    if (result.exitCode !== 0) {
      throw new ExecError('getScreenSize()', result.exitCode, result.stderr);
    }

    const parts = result.stdout.trim().split(/\s+/);
    if (parts.length === 2) {
      return [parseInt(parts[0], 10), parseInt(parts[1], 10)];
    }

    throw new ParseError('Failed to parse screen size from xdotool output');
  }
}
