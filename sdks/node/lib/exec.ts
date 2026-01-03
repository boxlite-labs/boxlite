/**
 * Result of a completed command execution.
 *
 * Contains the exit code and collected stdout/stderr output.
 * Used by SimpleBox and specialized box classes that collect
 * output into strings rather than streaming.
 *
 * @example
 * ```typescript
 * const result = await simpleBox.exec('ls', '-la');
 * console.log(`Exit code: ${result.exitCode}`);
 * console.log(`Output:\n${result.stdout}`);
 * if (result.stderr) {
 *   console.error(`Errors:\n${result.stderr}`);
 * }
 * ```
 */
export interface ExecResult {
  /**
   * Process exit code (0 = success, non-zero = error)
   */
  exitCode: number;

  /**
   * Standard output (stdout) as a string
   */
  stdout: string;

  /**
   * Standard error (stderr) as a string
   */
  stderr: string;
}
