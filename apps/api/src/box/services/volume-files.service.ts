/*
 * Copyright 2025 Daytona Platforms Inc.
 * Modified by BoxLite AI, 2025-2026
 * SPDX-License-Identifier: AGPL-3.0
 */

import { ConflictException, Injectable, Logger, NotFoundException, ServiceUnavailableException } from '@nestjs/common'
import {
  DeleteObjectCommand,
  DeleteObjectsCommand,
  GetObjectCommand,
  HeadObjectCommand,
  ListObjectsV2Command,
  PutObjectCommand,
  S3Client,
  S3ServiceException,
} from '@aws-sdk/client-s3'
import { getSignedUrl } from '@aws-sdk/s3-request-presigner'
import { VolumeService } from './volume.service'
import { VolumeState } from '../enums/volume-state.enum'
import { TypedConfigService } from '../../config/typed-config.service'
import { createS3Client } from '../../common/utils/s3-client.factory'
import { assertSafeVolumePath } from '../utils/volume-path.util'
import {
  BatchDeleteVolumeFilesResponseDto,
  BatchOperationErrorDto,
  ListVolumeFilesResponseDto,
  PresignBatchWriteVolumeFilesResponseDto,
  PresignedUrlResponseDto,
  VolumeFileEntryDto,
  VolumeFileStatDto,
} from '../dto/volume-file.dto'

/** S3 caps ListObjectsV2 / DeleteObjects at 1000 keys per call. */
const S3_MAX_KEYS_PER_CALL = 1000

/** How long a presigned URL stays valid before the caller must ask again. */
const PRESIGNED_URL_TTL_SECONDS = 900

/** Max concurrent presign operations in presignBatchWrite - bounds how long
 * one request can monopolize the event loop, independent of the DTO's
 * overall array-size cap. */
const PRESIGN_BATCH_CONCURRENCY = 50

@Injectable()
export class VolumeFilesService {
  private readonly logger = new Logger(VolumeFilesService.name)
  private readonly s3Client: S3Client | null

  constructor(
    private readonly volumeService: VolumeService,
    private readonly configService: TypedConfigService,
  ) {
    this.s3Client = createS3Client(this.configService)
  }

  async listFiles(volumeId: string, prefix: string, cursor?: string): Promise<ListVolumeFilesResponseDto> {
    if (prefix) {
      assertSafeVolumePath(prefix)
    }
    const volume = await this.assertReady(volumeId)

    const response = await this.s3.send(
      new ListObjectsV2Command({
        Bucket: volume.getBucketName(),
        Prefix: prefix || undefined,
        Delimiter: '/',
        MaxKeys: S3_MAX_KEYS_PER_CALL,
        ContinuationToken: cursor,
      }),
    )

    const directories: VolumeFileEntryDto[] = (response.CommonPrefixes ?? []).map((commonPrefix) => ({
      name: stripPrefix(commonPrefix.Prefix, prefix),
      isDirectory: true,
    }))
    const files: VolumeFileEntryDto[] = (response.Contents ?? [])
      // ListObjectsV2 returns the prefix "directory marker" object itself
      // (a zero-byte key ending in `/`) as a Contents entry too - it isn't a
      // real file the caller asked to see.
      .filter((object) => object.Key !== prefix)
      .map((object) => ({
        name: stripPrefix(object.Key, prefix),
        isDirectory: false,
        size: object.Size,
        lastModified: object.LastModified,
      }))

    return {
      entries: [...directories, ...files],
      nextCursor: response.NextContinuationToken,
      hasMore: response.IsTruncated ?? false,
    }
  }

  async statFile(volumeId: string, path: string): Promise<VolumeFileStatDto> {
    assertSafeVolumePath(path)
    const volume = await this.assertReady(volumeId)

    const head = await this.headObjectOrNotFound(volume.getBucketName(), path)
    return { path, size: head.ContentLength ?? 0, lastModified: head.LastModified }
  }

  async presignRead(volumeId: string, path: string): Promise<PresignedUrlResponseDto> {
    assertSafeVolumePath(path)
    const volume = await this.assertReady(volumeId)
    const bucket = volume.getBucketName()

    // Presigning never checks existence - sign now, fail at transfer time.
    // Doing our own existence check keeps the documented "404 if the path
    // is missing" contract instead of leaking that decision to S3's own
    // (delayed, harder for callers to detect) 404 on the actual GET.
    await this.headObjectOrNotFound(bucket, path)

    return this.presign(new GetObjectCommand({ Bucket: bucket, Key: path }))
  }

  async presignWrite(volumeId: string, path: string): Promise<PresignedUrlResponseDto> {
    assertSafeVolumePath(path)
    const volume = await this.assertReady(volumeId)

    // NOTE: single PutObject only - S3 hard-caps a single call at 5 GiB.
    // Presigned multipart upload (CreateMultipartUpload / per-part
    // presigned UploadPart / server-completed CompleteMultipartUpload) is
    // deliberately not implemented yet; writes above 5 GiB will fail at
    // the actual PUT against the presigned URL, not here. Tracked as
    // follow-up work rather than shipped half-verified.
    return this.presign(new PutObjectCommand({ Bucket: volume.getBucketName(), Key: path }))
  }

  async deleteFile(volumeId: string, path: string): Promise<void> {
    assertSafeVolumePath(path)
    const volume = await this.assertReady(volumeId)
    const bucket = volume.getBucketName()

    // DeleteObject is idempotent and succeeds even if the key never
    // existed; check first so a caller deleting a typo'd path gets 404
    // instead of a silent, misleading "success".
    await this.headObjectOrNotFound(bucket, path)
    await this.s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: path }))
  }

  async batchDelete(volumeId: string, paths: string[]): Promise<BatchDeleteVolumeFilesResponseDto> {
    paths.forEach(assertSafeVolumePath)
    const volume = await this.assertReady(volumeId)
    const bucket = volume.getBucketName()

    const deleted: string[] = []
    const errors: BatchOperationErrorDto[] = []

    for (const chunk of chunked(paths, S3_MAX_KEYS_PER_CALL)) {
      const response = await this.s3.send(
        new DeleteObjectsCommand({
          Bucket: bucket,
          Delete: { Objects: chunk.map((path) => ({ Key: path })), Quiet: false },
        }),
      )
      deleted.push(...(response.Deleted ?? []).map((object) => object.Key))
      errors.push(...(response.Errors ?? []).map((error) => ({ path: error.Key, message: error.Message })))
    }

    return { deleted, errors }
  }

  async presignBatchWrite(volumeId: string, paths: string[]): Promise<PresignBatchWriteVolumeFilesResponseDto> {
    paths.forEach(assertSafeVolumePath)
    const volume = await this.assertReady(volumeId)
    const bucket = volume.getBucketName()

    const urls: PresignBatchWriteVolumeFilesResponseDto['urls'] = []
    const errors: BatchOperationErrorDto[] = []

    // Presigning is a local signature computation, not a network call, so
    // there's no batching primitive to reach for here the way there is for
    // DeleteObjects. DTO validation caps `paths` at 1000, but firing all of
    // them through one Promise.all would still run the whole batch as a
    // single microtask burst with no chance for other requests to interleave
    // on the event loop - process in bounded chunks instead.
    for (const chunk of chunked(paths, PRESIGN_BATCH_CONCURRENCY)) {
      await Promise.all(
        chunk.map(async (path) => {
          try {
            const { url, expiresAt } = await this.presign(new PutObjectCommand({ Bucket: bucket, Key: path }))
            urls.push({ path, url, expiresAt })
          } catch (error) {
            errors.push({ path, message: error instanceof Error ? error.message : String(error) })
          }
        }),
      )
    }

    return { urls, errors }
  }

  private async assertReady(volumeId: string) {
    const volume = await this.volumeService.findOne(volumeId)
    if (volume.state !== VolumeState.READY) {
      throw new ConflictException(`invalid state: volume is ${volume.state}`)
    }
    return volume
  }

  private async headObjectOrNotFound(bucket: string, path: string) {
    try {
      return await this.s3.send(new HeadObjectCommand({ Bucket: bucket, Key: path }))
    } catch (error) {
      if (error instanceof S3ServiceException && error.$metadata?.httpStatusCode === 404) {
        throw new NotFoundException(`file not found: ${path}`)
      }
      throw error
    }
  }

  private async presign(command: GetObjectCommand | PutObjectCommand): Promise<PresignedUrlResponseDto> {
    const url = await getSignedUrl(this.s3, command, { expiresIn: PRESIGNED_URL_TTL_SECONDS })
    return { url, expiresAt: new Date(Date.now() + PRESIGNED_URL_TTL_SECONDS * 1000) }
  }

  private get s3(): S3Client {
    if (!this.s3Client) {
      throw new ServiceUnavailableException('Object storage is not configured')
    }
    return this.s3Client
  }
}

function stripPrefix(key: string | undefined, prefix: string): string {
  return (key ?? '').slice(prefix.length)
}

function chunked<T>(items: T[], size: number): T[][] {
  const chunks: T[][] = []
  for (let i = 0; i < items.length; i += size) {
    chunks.push(items.slice(i, i + size))
  }
  return chunks
}
