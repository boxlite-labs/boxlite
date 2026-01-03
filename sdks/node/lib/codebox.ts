/**
 * CodeBox - Secure Python code execution container.
 *
 * Provides a simple, secure environment for running untrusted Python code.
 */

import { SimpleBox, type SimpleBoxOptions } from './simplebox';
import { ExecError } from './errors';

/**
 * Options for creating a CodeBox.
 */
export interface CodeBoxOptions extends Omit<SimpleBoxOptions, 'image'> {
  /** Container image with Python (default: 'python:slim') */
  image?: string;
}

/**
 * Secure container for executing Python code.
 *
 * CodeBox provides an isolated environment for running untrusted Python code
 * with built-in safety and result formatting.
 *
 * ## Usage
 *
 * ```typescript
 * const codebox = new CodeBox();
 * try {
 *   const result = await codebox.run("print('Hello, World!')");
 *   console.log(result);  // Hello, World!
 * } finally {
 *   await codebox.stop();
 * }
 * ```
 *
 * Or with async disposal (TypeScript 5.2+):
 *
 * ```typescript
 * await using codebox = new CodeBox();
 * const result = await codebox.run("print('Hello, World!')");
 * // Automatically stopped when leaving scope
 * ```
 */
export class CodeBox extends SimpleBox {
  /**
   * Create a new CodeBox.
   *
   * @param options - CodeBox configuration options
   *
   * @example
   * ```typescript
   * const codebox = new CodeBox({
   *   image: 'python:slim',
   *   memoryMib: 512,
   *   cpus: 2
   * });
   * ```
   */
  constructor(options: CodeBoxOptions = {}) {
    super({
      ...options,
      image: options.image ?? 'python:slim',
    });
  }

  /**
   * Execute Python code in the secure container.
   *
   * @param code - Python code to execute
   *
   * @returns Promise resolving to stdout output
   * @throws {ExecError} If Python execution fails (non-zero exit code)
   *
   * @example
   * ```typescript
   * const codebox = new CodeBox();
   * try {
   *   const result = await codebox.run("print('Hello, World!')");
   *   console.log(result);  // Hello, World!
   * } finally {
   *   await codebox.stop();
   * }
   * ```
   *
   * @remarks
   * Uses python3 from the container image.
   * For custom Python paths, use exec() directly:
   * ```typescript
   * const result = await codebox.exec('/path/to/python', '-c', code);
   * ```
   */
  async run(code: string): Promise<string> {
    const result = await this.exec('/usr/local/bin/python', '-c', code);
    if (result.exitCode !== 0) {
      throw new ExecError('run()', result.exitCode, result.stderr);
    }
    return result.stdout;
  }

  /**
   * Execute a Python script file in the container.
   *
   * @param scriptPath - Path to the Python script on the host
   *
   * @returns Promise resolving to execution output
   *
   * @example
   * ```typescript
   * const codebox = new CodeBox();
   * try {
   *   const result = await codebox.runScript('./my_script.py');
   *   console.log(result);
   * } finally {
   *   await codebox.stop();
   * }
   * ```
   */
  async runScript(scriptPath: string): Promise<string> {
    const fs = await import('fs/promises');
    const code = await fs.readFile(scriptPath, 'utf-8');
    return this.run(code);
  }

  /**
   * Install a Python package in the container using pip.
   *
   * @param packageName - Package name (e.g., 'requests', 'numpy==1.24.0')
   *
   * @returns Promise resolving to installation output
   * @throws {ExecError} If pip installation fails
   *
   * @example
   * ```typescript
   * const codebox = new CodeBox();
   * try {
   *   await codebox.installPackage('requests');
   *   const result = await codebox.run('import requests; print(requests.__version__)');
   *   console.log(result);
   * } finally {
   *   await codebox.stop();
   * }
   * ```
   */
  async installPackage(packageName: string): Promise<string> {
    const result = await this.exec('pip', 'install', packageName);
    if (result.exitCode !== 0) {
      throw new ExecError(`installPackage('${packageName}')`, result.exitCode, result.stderr);
    }
    return result.stdout;
  }

  /**
   * Install multiple Python packages.
   *
   * @param packages - Package names to install
   *
   * @returns Promise resolving to installation output
   * @throws {ExecError} If pip installation fails
   *
   * @example
   * ```typescript
   * const codebox = new CodeBox();
   * try {
   *   await codebox.installPackages('requests', 'numpy', 'pandas');
   *   const result = await codebox.run('import requests, numpy, pandas');
   * } finally {
   *   await codebox.stop();
   * }
   * ```
   */
  async installPackages(...packages: string[]): Promise<string> {
    const result = await this.exec('pip', 'install', ...packages);
    if (result.exitCode !== 0) {
      throw new ExecError(`installPackages(${packages.join(', ')})`, result.exitCode, result.stderr);
    }
    return result.stdout;
  }
}
