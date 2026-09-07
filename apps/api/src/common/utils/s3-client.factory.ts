/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { S3Client } from '@aws-sdk/client-s3'
import { TypedConfigService } from '../../config/typed-config.service'

/**
 * Builds the S3-compatible client shared by every volume feature (bucket
 * lifecycle in `VolumeManager`, file access in `VolumeFilesService`).
 * Extracted here so both stay in sync instead of drifting on config keys,
 * MinIO's credential requirement, or the both-or-neither key check.
 *
 * Returns `null` when object storage isn't configured (`s3.endpoint` unset) -
 * callers that need a client should throw their own domain-appropriate error
 * (e.g. `ServiceUnavailableException`) rather than this factory choosing one.
 */
export function createS3Client(configService: TypedConfigService): S3Client | null {
  if (!configService.get('s3.endpoint')) {
    return null
  }

  const endpoint = configService.getOrThrow('s3.endpoint')
  const region = configService.getOrThrow('s3.region')
  const accessKeyId = configService.get('s3.accessKey')
  const secretAccessKey = configService.get('s3.secretKey')

  // Both-or-neither: a lone key is a typo'd pair, and silently falling back
  // to the SDK default chain would mask the misconfig.
  if ((accessKeyId && !secretAccessKey) || (!accessKeyId && secretAccessKey)) {
    throw new Error('S3_ACCESS_KEY and S3_SECRET_KEY must be set together')
  }
  // MinIO cannot use the SDK default chain - fail fast at boot with a clear
  // message instead of a generic auth error from the connection probe.
  if (endpoint.includes('minio') && !accessKeyId) {
    throw new Error('MinIO requires S3_ACCESS_KEY and S3_SECRET_KEY to be configured')
  }

  return new S3Client({
    endpoint: withScheme(endpoint),
    region,
    // Static keys for S3-compatible deployments (MinIO); unset on AWS,
    // where the SDK default chain supplies the ECS task-role credentials.
    ...(accessKeyId && secretAccessKey ? { credentials: { accessKeyId, secretAccessKey } } : {}),
    forcePathStyle: true,
  })
}

/**
 * Adds a URL scheme to a bare host:port endpoint. Defaults to HTTPS - this
 * client now also signs presigned URLs handed straight to external callers
 * (`VolumeFilesService`), so a scheme-less production endpoint must not
 * silently downgrade every read/write to plaintext. Only known local/dev
 * deployments (MinIO, localhost) fall back to HTTP, matching the existing
 * MinIO carve-out for the credential requirement above.
 */
function withScheme(endpoint: string): string {
  if (endpoint.startsWith('http://') || endpoint.startsWith('https://')) {
    return endpoint
  }
  const isLocal = endpoint.includes('minio') || endpoint.includes('localhost') || endpoint.includes('127.0.0.1')
  return `${isLocal ? 'http' : 'https'}://${endpoint}`
}
