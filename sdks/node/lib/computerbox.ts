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
   * Execute a command and throw ExecError if it fails.
   *
   * @param label - Label for error messages (e.g., 'mouseMove(100, 200)')
   * @param cmd - Command to execute
   * @param args - Command arguments
   * @returns The execution result (for methods that need to parse output)
   * @throws {ExecError} If command exits with non-zero code
   */
  private async execOrThrow(label: string, cmd: string, ...args: string[]): Promise<{ exitCode: number; stdout: string; stderr: string }> {
    const result = await this.exec(cmd, ...args);
    if (result.exitCode !== 0) {
      throw new ExecError(label, result.exitCode, result.stderr);
    }
    return result;
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

    const result = await this.execOrThrow('screenshot()', 'python3', '-c', pythonCode);

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
    await this.execOrThrow(`mouseMove(${x}, ${y})`, 'xdotool', 'mousemove', x.toString(), y.toString());
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
    await this.execOrThrow('leftClick()', 'xdotool', 'click', '1');
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
    await this.execOrThrow('rightClick()', 'xdotool', 'click', '3');
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
    await this.execOrThrow('middleClick()', 'xdotool', 'click', '2');
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
    await this.execOrThrow('doubleClick()', 'xdotool', 'click', '--repeat', '2', '--delay', '100', '1');
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
    await this.execOrThrow('tripleClick()', 'xdotool', 'click', '--repeat', '3', '--delay', '100', '1');
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
    await this.execOrThrow(
      'leftClickDrag()',
      'xdotool',
      'mousemove', startX.toString(), startY.toString(),
      'mousedown', '1',
      'sleep', '0.1',
      'mousemove', endX.toString(), endY.toString(),
      'sleep', '0.1',
      'mouseup', '1'
    );
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
    const result = await this.execOrThrow('cursorPosition()', 'xdotool', 'getmouselocation', '--shell');

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
    await this.execOrThrow('type()', 'xdotool', 'type', '--', text);
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
    await this.execOrThrow('key()', 'xdotool', 'key', keySequence);
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

    await this.execOrThrow(
      'scroll()',
      'xdotool', 'mousemove', x.toString(), y.toString(), 'click', '--repeat', amount.toString(), button
    );
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
    const result = await this.execOrThrow('getScreenSize()', 'xdotool', 'getdisplaygeometry');

    const parts = result.stdout.trim().split(/\s+/);
    if (parts.length === 2) {
      return [parseInt(parts[0], 10), parseInt(parts[1], 10)];
    }

    throw new ParseError('Failed to parse screen size from xdotool output');
  }
}
