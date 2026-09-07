/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import * as path from 'path'
import { BadRequestError } from '../../exceptions/bad-request.exception'

/**
 * Validates a caller-supplied volume file path before it becomes an S3 key.
 *
 * Mirrors the zip-slip guard already used for box file uploads
 * (`apps/runner/pkg/api/controllers/boxlite_files.go:96-99`): reject an
 * absolute path or one that escapes upward via `..`, rather than silently
 * normalizing it. Kept as a standalone assertion (not path building) so it
 * can validate at every entry point without also deciding how the caller's
 * path is joined to a bucket prefix.
 */
export function assertSafeVolumePath(rawPath: string): void {
  if (!rawPath) {
    throw new BadRequestError('path must not be empty')
  }

  const cleaned = path.posix.normalize(rawPath)

  if (path.posix.isAbsolute(cleaned) || cleaned === '..' || cleaned.startsWith('../')) {
    throw new BadRequestError(`path escapes the volume root: ${rawPath}`)
  }
}
