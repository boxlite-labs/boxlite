/**
 * BoxLite Node.js SDK
 *
 * Embeddable VM runtime for secure, isolated code execution environments.
 *
 * @example
 * ```typescript
 * import { SimpleBox } from '@boxlite/boxlite';
 *
 * const box = new SimpleBox({ image: 'alpine:latest' });
 * try {
 *   const result = await box.exec('echo', 'Hello from BoxLite!');
 *   console.log(result.stdout);
 * } finally {
 *   await box.stop();
 * }
 * ```
 *
 * @packageDocumentation
 */

import { getNativeModule, getJsBoxlite } from './native';

// Re-export native bindings
export const JsBoxlite = getJsBoxlite();

// Export native module loader for advanced use cases
export { getNativeModule, getJsBoxlite };

// Re-export TypeScript wrappers
export { SimpleBox, type SimpleBoxOptions } from './simplebox';
export { type ExecResult } from './exec';
export { BoxliteError, ExecError, TimeoutError, ParseError } from './errors';
export * from './constants';

// Specialized boxes
export { CodeBox, type CodeBoxOptions } from './codebox';
export { BrowserBox, type BrowserBoxOptions, type BrowserType } from './browserbox';
export { ComputerBox, type ComputerBoxOptions, type Screenshot } from './computerbox';
export { InteractiveBox, type InteractiveBoxOptions } from './interactivebox';
