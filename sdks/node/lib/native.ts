/**
 * Native module loader for BoxLite Node.js SDK.
 *
 * This module centralizes native binding loading to avoid duplication
 * across simplebox.ts, interactivebox.ts, and index.ts.
 */

import { loadBinding } from '@node-rs/helper';
import { join } from 'path';

// Cache the loaded native module
let _nativeModule: any = null;

/**
 * Load and return the native BoxLite module.
 *
 * Uses @node-rs/helper for platform-specific package selection.
 * Caches the result for subsequent calls.
 *
 * @throws Error if the native extension is not found or fails to load
 */
export function getNativeModule(): any {
  if (_nativeModule === null) {
    try {
      _nativeModule = loadBinding(join(__dirname, '..'), 'boxlite', '@boxlite/boxlite');
    } catch (err) {
      throw new Error(
        `BoxLite native extension not found: ${err}. ` +
        `Please build the extension first with: npm run build`
      );
    }
  }
  return _nativeModule;
}

/**
 * Get the JsBoxlite runtime class from the native module.
 */
export function getJsBoxlite(): any {
  return getNativeModule().JsBoxlite;
}
