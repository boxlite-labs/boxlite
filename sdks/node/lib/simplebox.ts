/**
 * SimpleBox - Foundation for specialized container types.
 *
 * Provides common functionality for all specialized boxes (CodeBox, BrowserBox, etc.)
 * This class encapsulates common patterns:
 * 1. Automatic runtime lifecycle management
 * 2. Command execution with output collection
 * 3. Try/finally cleanup patterns
 */

import type { ExecResult } from './exec';
import { getJsBoxlite } from './native';

// Import types from native module (will be available after build)
type Boxlite = any;
type Box = any;
type Execution = any;
type BoxOptions = any;

/**
 * Options for creating a SimpleBox.
 */
export interface SimpleBoxOptions {
  /** Container image to use (e.g., 'python:slim', 'alpine:latest') */
  image?: string;

  /** Memory limit in MiB */
  memoryMib?: number;

  /** Number of CPU cores */
  cpus?: number;

  /** Optional runtime instance (uses global default if not provided) */
  runtime?: Boxlite;

  /** Optional name for the box (must be unique) */
  name?: string;

  /** Remove box when stopped (default: true) */
  autoRemove?: boolean;

  /** Run box in detached mode (survives parent process exit, default: false) */
  detach?: boolean;

  /** Working directory inside container */
  workingDir?: string;

  /** Environment variables */
  env?: Record<string, string>;

  /** Volume mounts */
  volumes?: Array<{
    hostPath: string;
    guestPath: string;
    readOnly?: boolean;
  }>;

  /** Port mappings */
  ports?: Array<{
    hostPort?: number;
    guestPort: number;
    protocol?: string;
  }>;
}

/**
 * Base class for specialized container types.
 *
 * This class provides the foundation for all specialized boxes:
 * - CodeBox: Python code execution sandbox
 * - BrowserBox: Browser automation
 * - ComputerBox: Desktop automation
 * - InteractiveBox: PTY terminal sessions
 *
 * ## Usage
 *
 * SimpleBox can be used directly for simple command execution:
 *
 * ```typescript
 * const box = new SimpleBox({ image: 'alpine:latest' });
 * try {
 *   const result = await box.exec('ls', '-la', '/');
 *   console.log(result.stdout);
 * } finally {
 *   await box.stop();
 * }
 * ```
 *
 * Or extended for specialized use cases:
 *
 * ```typescript
 * class MyBox extends SimpleBox {
 *   constructor() {
 *     super({ image: 'my-custom-image:latest' });
 *   }
 *
 *   async myMethod() {
 *     const result = await this.exec('my-command');
 *     return result.stdout;
 *   }
 * }
 * ```
 */
export class SimpleBox {
  protected _runtime: Boxlite;
  protected _box: Box;
  protected _name?: string;

  /**
   * Create a new SimpleBox.
   *
   * @param options - Box configuration options
   *
   * @example
   * ```typescript
   * const box = new SimpleBox({
   *   image: 'python:slim',
   *   memoryMib: 512,
   *   cpus: 2,
   *   name: 'my-box'
   * });
   * ```
   */
  constructor(options: SimpleBoxOptions = {}) {
    const JsBoxlite = getJsBoxlite();

    // Use provided runtime or get global default
    if (options.runtime) {
      this._runtime = options.runtime;
    } else {
      this._runtime = JsBoxlite.default();
    }

    // Convert options to BoxOptions format
    const boxOpts: BoxOptions = {
      image: options.image,
      cpus: options.cpus,
      memoryMib: options.memoryMib,
      autoRemove: options.autoRemove ?? true,
      detach: options.detach ?? false,
      workingDir: options.workingDir,
      env: options.env
        ? Object.entries(options.env).map(([key, value]) => ({ key, value }))
        : undefined,
      volumes: options.volumes,
      ports: options.ports,
    };

    this._name = options.name;
    this._box = this._runtime.create(boxOpts, options.name);
  }

  /**
   * Get the box ID (ULID format).
   */
  get id(): string {
    return this._box.id;
  }

  /**
   * Get the box name (if set).
   */
  get name(): string | undefined {
    return this._name;
  }

  /**
   * Get box metadata.
   */
  info() {
    return this._box.info();
  }

  /**
   * Execute a command in the box and collect the output.
   *
   * This is a convenience method that:
   * 1. Starts the command
   * 2. Collects all stdout and stderr
   * 3. Waits for completion
   * 4. Returns the result
   *
   * For streaming output, use the lower-level `this._box.exec()` directly.
   *
   * @param cmd - Command to execute (e.g., 'ls', 'python')
   * @param args - Arguments to the command
   * @param env - Environment variables (optional)
   *
   * @returns Promise resolving to ExecResult with exit code and output
   *
   * @example
   * ```typescript
   * // Simple execution
   * const result = await box.exec('ls', '-la', '/');
   * console.log(`Exit code: ${result.exitCode}`);
   * console.log(`Output:\n${result.stdout}`);
   *
   * // With environment variables
   * const result = await box.exec('env', [], { FOO: 'bar' });
   * console.log(result.stdout);
   * ```
   */
  async exec(cmd: string, ...args: string[]): Promise<ExecResult>;
  async exec(cmd: string, args: string[], env: Record<string, string>): Promise<ExecResult>;
  async exec(
    cmd: string,
    argsOrFirstArg?: string | string[],
    envOrSecondArg?: Record<string, string> | string,
    ...restArgs: string[]
  ): Promise<ExecResult> {
    // Parse overloaded arguments
    let args: string[];
    let env: Record<string, string> | undefined;

    if (Array.isArray(argsOrFirstArg)) {
      // exec(cmd, args[], env?)
      args = argsOrFirstArg;
      env = envOrSecondArg as Record<string, string> | undefined;
    } else {
      // exec(cmd, ...args, env?)
      // Collect all arguments
      const allArgs: any[] = [
        argsOrFirstArg,
        envOrSecondArg,
        ...restArgs,
      ].filter(a => a !== undefined);

      // Check if last arg is env object (before filtering to strings)
      const lastArg = allArgs[allArgs.length - 1];
      if (lastArg && typeof lastArg === 'object' && !Array.isArray(lastArg)) {
        env = lastArg as Record<string, string>;
        args = allArgs.slice(0, -1).filter((a): a is string => typeof a === 'string');
      } else {
        env = undefined;
        args = allArgs.filter((a): a is string => typeof a === 'string');
      }
    }

    // Convert env to array of tuples
    const envArray = env
      ? Object.entries(env).map(([k, v]) => [k, v] as [string, string])
      : undefined;

    // Execute via Rust (returns Execution)
    const execution: Execution = await this._box.exec(cmd, args, envArray, false);

    // Collect stdout and stderr
    const stdoutLines: string[] = [];
    const stderrLines: string[] = [];

    // Get streams
    let stdout;
    let stderr;

    try {
      stdout = execution.stdout();
    } catch (err) {
      // Stream not available (expected for some commands)
      stdout = null;
    }

    try {
      stderr = execution.stderr();
    } catch (err) {
      // Stream not available (expected for some commands)
      stderr = null;
    }

    // Read stdout
    if (stdout) {
      try {
        while (true) {
          const line = await stdout.next();
          if (line === null) break;
          stdoutLines.push(line);
        }
      } catch (err) {
        // Stream ended or error occurred
      }
    }

    // Read stderr
    if (stderr) {
      try {
        while (true) {
          const line = await stderr.next();
          if (line === null) break;
          stderrLines.push(line);
        }
      } catch (err) {
        // Stream ended or error occurred
      }
    }

    // Wait for completion
    const result = await execution.wait();

    return {
      exitCode: result.exitCode,
      stdout: stdoutLines.join(''),
      stderr: stderrLines.join(''),
    };
  }

  /**
   * Stop the box.
   *
   * Sends a graceful shutdown signal to the VM. If `autoRemove` is true
   * (default), the box files will be deleted after stopping.
   *
   * @example
   * ```typescript
   * await box.stop();
   * console.log('Box stopped');
   * ```
   */
  async stop(): Promise<void> {
    await this._box.stop();
  }

  /**
   * Implement async disposable pattern (TypeScript 5.2+).
   *
   * Allows using `await using` syntax for automatic cleanup:
   *
   * ```typescript
   * await using box = new SimpleBox({ image: 'alpine' });
   * // Box automatically stopped when leaving scope
   * ```
   */
  async [Symbol.asyncDispose](): Promise<void> {
    await this.stop();
  }
}
