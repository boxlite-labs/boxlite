/**
 * BrowserBox - Secure browser with remote debugging.
 *
 * Provides a minimal, elegant API for running isolated browsers that can be
 * controlled from outside using standard tools like Puppeteer or Playwright.
 */

import { SimpleBox, type SimpleBoxOptions } from './simplebox';
import { TimeoutError } from './errors';
import * as constants from './constants';

/**
 * Browser type supported by BrowserBox.
 */
export type BrowserType = 'chromium' | 'firefox' | 'webkit';

/**
 * Options for creating a BrowserBox.
 */
export interface BrowserBoxOptions extends Omit<SimpleBoxOptions, 'image' | 'cpus' | 'memoryMib'> {
  /** Browser type (default: 'chromium') */
  browser?: BrowserType;

  /** Memory in MiB (default: 2048) */
  memoryMib?: number;

  /** Number of CPU cores (default: 2) */
  cpus?: number;
}

/**
 * Secure browser environment with remote debugging.
 *
 * Auto-starts a browser with Chrome DevTools Protocol enabled.
 * Connect from outside using Puppeteer, Playwright, Selenium, or DevTools.
 *
 * ## Usage
 *
 * ```typescript
 * const browser = new BrowserBox();
 * try {
 *   await browser.start();  // Manually start browser
 *   console.log(`Connect to: ${browser.endpoint()}`);
 *   // Use Puppeteer/Playwright from your host to connect
 *   await new Promise(resolve => setTimeout(resolve, 60000));
 * } finally {
 *   await browser.stop();
 * }
 * ```
 *
 * ## Example with custom options
 *
 * ```typescript
 * const browser = new BrowserBox({
 *   browser: 'firefox',
 *   memoryMib: 4096,
 *   cpus: 4
 * });
 * try {
 *   await browser.start();
 *   const endpoint = browser.endpoint();
 *   // Connect using Playwright or Puppeteer
 * } finally {
 *   await browser.stop();
 * }
 * ```
 */
export class BrowserBox extends SimpleBox {
  private static readonly DEFAULT_IMAGE = 'mcr.microsoft.com/playwright:v1.47.2-jammy';

  private static readonly PORTS: Record<BrowserType, number> = {
    chromium: constants.BROWSERBOX_PORT_CHROMIUM,
    firefox: constants.BROWSERBOX_PORT_FIREFOX,
    webkit: constants.BROWSERBOX_PORT_WEBKIT,
  };

  private readonly _browser: BrowserType;
  private readonly _port: number;

  /**
   * Create a new BrowserBox.
   *
   * @param options - BrowserBox configuration options
   *
   * @example
   * ```typescript
   * const browser = new BrowserBox({
   *   browser: 'chromium',
   *   memoryMib: 2048,
   *   cpus: 2
   * });
   * ```
   */
  constructor(options: BrowserBoxOptions = {}) {
    const {
      browser = 'chromium',
      memoryMib = 2048,
      cpus = 2,
      ...restOptions
    } = options;

    super({
      ...restOptions,
      image: BrowserBox.DEFAULT_IMAGE,
      memoryMib,
      cpus,
    });

    this._browser = browser;
    this._port = BrowserBox.PORTS[browser] || 9222;
  }

  /**
   * Start the browser with remote debugging enabled.
   *
   * Call this method after creating the BrowserBox to start the browser process.
   * Waits for the browser to be ready before returning.
   *
   * @param timeout - Maximum time to wait for browser to start in seconds (default: 30)
   * @throws {TimeoutError} If browser doesn't start within timeout
   *
   * @example
   * ```typescript
   * const browser = new BrowserBox();
   * try {
   *   await browser.start();
   *   console.log(`Connect to: ${browser.endpoint()}`);
   * } finally {
   *   await browser.stop();
   * }
   * ```
   */
  async start(timeout: number = 30): Promise<void> {
    let cmd: string;
    let processPattern: string;

    if (this._browser === 'chromium') {
      const binary = '/ms-playwright/chromium-*/chrome-linux/chrome';
      cmd =
        `${binary} --headless --no-sandbox --disable-dev-shm-usage ` +
        `--disable-gpu --remote-debugging-address=0.0.0.0 ` +
        `--remote-debugging-port=${this._port} ` +
        `> /tmp/browser.log 2>&1 &`;
      processPattern = 'chrome';
    } else if (this._browser === 'firefox') {
      const binary = '/ms-playwright/firefox-*/firefox/firefox';
      cmd =
        `${binary} --headless ` +
        `--remote-debugging-port=${this._port} ` +
        `> /tmp/browser.log 2>&1 &`;
      processPattern = 'firefox';
    } else {
      // webkit
      cmd =
        `playwright run-server --browser webkit ` +
        `--port ${this._port} > /tmp/browser.log 2>&1 &`;
      processPattern = 'playwright';
    }

    // Start browser in background
    await this.exec('sh', '-c', `nohup ${cmd}`);

    // Wait for browser to be ready
    await this.waitForBrowser(processPattern, timeout);
  }

  /**
   * Wait for the browser process to be running.
   *
   * @param processPattern - Pattern to match browser process name
   * @param timeout - Maximum wait time in seconds
   * @throws {TimeoutError} If browser doesn't start within timeout
   */
  private async waitForBrowser(processPattern: string, timeout: number): Promise<void> {
    const startTime = Date.now();
    const pollInterval = 0.5;

    while (true) {
      const elapsed = (Date.now() - startTime) / 1000;
      if (elapsed > timeout) {
        throw new TimeoutError(`Browser '${this._browser}' did not start within ${timeout} seconds`);
      }

      // Check if browser process is running
      const result = await this.exec('pgrep', '-f', processPattern);
      if (result.exitCode === 0 && result.stdout.trim()) {
        // Browser process found, give it a moment to initialize
        await new Promise(resolve => setTimeout(resolve, 500));
        return;
      }

      // Wait before retrying
      await new Promise(resolve => setTimeout(resolve, pollInterval * 1000));
    }
  }

  /**
   * Get the connection endpoint for remote debugging.
   *
   * Returns the HTTP endpoint URL for Chrome DevTools Protocol.
   * Use this with Puppeteer, Playwright, or Selenium to connect to the browser.
   *
   * @returns HTTP endpoint URL (e.g., 'http://localhost:9222')
   *
   * @example
   * ```typescript
   * const browser = new BrowserBox();
   * try {
   *   await browser.start();
   *   const url = browser.endpoint();
   *
   *   // Use with Puppeteer:
   *   // const browserWSEndpoint = await fetch(`${url}/json/version`)
   *   //   .then(r => r.json())
   *   //   .then(d => d.webSocketDebuggerUrl);
   *   // const browser = await puppeteer.connect({ browserWSEndpoint });
   *
   *   // Use with Playwright:
   *   // const browser = await chromium.connectOverCDP(url);
   * } finally {
   *   await browser.stop();
   * }
   * ```
   */
  endpoint(): string {
    return `http://localhost:${this._port}`;
  }

  /**
   * Override async disposal to start browser automatically.
   *
   * @internal
   */
  async [Symbol.asyncDispose](): Promise<void> {
    await super[Symbol.asyncDispose]();
  }
}
