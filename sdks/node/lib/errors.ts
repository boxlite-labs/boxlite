/**
 * Base error class for all BoxLite-related errors.
 *
 * All BoxLite errors inherit from this class, allowing easy error type checking:
 * ```typescript
 * try {
 *   await box.exec('invalid-command');
 * } catch (err) {
 *   if (err instanceof BoxliteError) {
 *     console.error('BoxLite error:', err.message);
 *   }
 * }
 * ```
 */
export class BoxliteError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'BoxliteError';
    // Maintain proper stack trace for where our error was thrown (V8 only)
    if (Error.captureStackTrace) {
      Error.captureStackTrace(this, BoxliteError);
    }
  }
}

/**
 * Execution error thrown when a command fails (non-zero exit code).
 *
 * Contains details about the failed command, exit code, and stderr output.
 *
 * @example
 * ```typescript
 * try {
 *   const result = await box.exec('false');
 * } catch (err) {
 *   if (err instanceof ExecError) {
 *     console.error(`Command '${err.command}' failed with exit code ${err.exitCode}`);
 *     console.error(`Stderr: ${err.stderr}`);
 *   }
 * }
 * ```
 */
export class ExecError extends BoxliteError {
  /**
   * @param command - The command that failed
   * @param exitCode - The non-zero exit code
   * @param stderr - Standard error output
   */
  constructor(
    public readonly command: string,
    public readonly exitCode: number,
    public readonly stderr: string
  ) {
    super(`Command '${command}' failed with exit code ${exitCode}: ${stderr}`);
    this.name = 'ExecError';
    if (Error.captureStackTrace) {
      Error.captureStackTrace(this, ExecError);
    }
  }
}

/**
 * Timeout error thrown when an operation exceeds its time limit.
 *
 * @example
 * ```typescript
 * try {
 *   await waitForDesktopReady(box, 60); // 60 second timeout
 * } catch (err) {
 *   if (err instanceof TimeoutError) {
 *     console.error('Operation timed out:', err.message);
 *   }
 * }
 * ```
 */
export class TimeoutError extends BoxliteError {
  constructor(message: string) {
    super(message);
    this.name = 'TimeoutError';
    if (Error.captureStackTrace) {
      Error.captureStackTrace(this, TimeoutError);
    }
  }
}

/**
 * Parse error thrown when unable to parse command output.
 *
 * Used when parsing structured output (JSON, coordinates, etc.) fails.
 *
 * @example
 * ```typescript
 * try {
 *   const position = parseCursorPosition(output);
 * } catch (err) {
 *   if (err instanceof ParseError) {
 *     console.error('Failed to parse output:', err.message);
 *   }
 * }
 * ```
 */
export class ParseError extends BoxliteError {
  constructor(message: string) {
    super(message);
    this.name = 'ParseError';
    if (Error.captureStackTrace) {
      Error.captureStackTrace(this, ParseError);
    }
  }
}
