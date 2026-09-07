import { ConflictException, NotFoundException } from '@nestjs/common'
import { S3ServiceException } from '@aws-sdk/client-s3'
import { VolumeFilesService } from './volume-files.service'
import { VolumeState } from '../enums/volume-state.enum'

jest.mock('../../common/utils/s3-client.factory', () => ({
  createS3Client: jest.fn(),
}))
jest.mock('@aws-sdk/s3-request-presigner', () => ({
  getSignedUrl: jest.fn(),
}))

import { createS3Client } from '../../common/utils/s3-client.factory'
import { getSignedUrl } from '@aws-sdk/s3-request-presigner'

afterEach(() => jest.clearAllMocks())

function notFoundError(): S3ServiceException {
  const error = new S3ServiceException({
    name: 'NotFound',
    $fault: 'client',
    $metadata: { httpStatusCode: 404 },
  })
  return error
}

function createService(volume: { id: string; state: VolumeState } | null) {
  const send = jest.fn()
  const s3Client = { send }
  ;(createS3Client as jest.Mock).mockReturnValue(s3Client)

  const volumeService = {
    findOne: jest.fn().mockImplementation((id: string) => {
      if (!volume || volume.id !== id) {
        throw new NotFoundException(`Volume with ID ${id} not found`)
      }
      return { ...volume, getBucketName: () => `boxlite-volume-${volume.id}` }
    }),
  }

  const service = new VolumeFilesService(volumeService as never, {} as never)
  return { service, send, volumeService }
}

const READY_VOLUME = { id: 'volume-1', state: VolumeState.READY }

describe('VolumeFilesService readiness gate', () => {
  it('rejects an operation on a non-READY volume with 409 naming the state', async () => {
    const { service } = createService({ id: 'volume-1', state: VolumeState.CREATING })

    await expect(service.statFile('volume-1', 'a.txt')).rejects.toMatchObject({
      constructor: ConflictException,
      message: expect.stringContaining('volume is creating'),
    })
  })
})

describe('VolumeFilesService path validation', () => {
  it('rejects a traversal path before touching S3', async () => {
    const { service, send } = createService(READY_VOLUME)

    await expect(service.statFile('volume-1', '../secret')).rejects.toThrow('escapes the volume root')
    expect(send).not.toHaveBeenCalled()
  })
})

describe('VolumeFilesService listFiles', () => {
  it('maps common prefixes and objects into entries, and passes through pagination', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockResolvedValue({
      CommonPrefixes: [{ Prefix: 'checkpoints/logs/' }],
      Contents: [
        { Key: 'checkpoints/', Size: 0 }, // directory marker - must be filtered out
        { Key: 'checkpoints/model.bin', Size: 1024, LastModified: new Date('2026-01-01') },
      ],
      NextContinuationToken: 'cursor-2',
      IsTruncated: true,
    })

    const result = await service.listFiles('volume-1', 'checkpoints/', 'cursor-1')

    expect(result).toEqual({
      entries: [
        { name: 'logs/', isDirectory: true },
        { name: 'model.bin', isDirectory: false, size: 1024, lastModified: new Date('2026-01-01') },
      ],
      nextCursor: 'cursor-2',
      hasMore: true,
    })
    expect(send.mock.calls[0][0].input).toMatchObject({
      Bucket: 'boxlite-volume-volume-1',
      Prefix: 'checkpoints/',
      Delimiter: '/',
      ContinuationToken: 'cursor-1',
    })
  })

  it('rejects a traversal prefix before touching S3', async () => {
    const { service, send } = createService(READY_VOLUME)

    await expect(service.listFiles('volume-1', '../secret')).rejects.toThrow('escapes the volume root')
    expect(send).not.toHaveBeenCalled()
  })

  it('does not validate the default empty prefix (volume root listing)', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockResolvedValue({ Contents: [], IsTruncated: false })

    await expect(service.listFiles('volume-1', '')).resolves.toEqual({
      entries: [],
      nextCursor: undefined,
      hasMore: false,
    })
  })
})

describe('VolumeFilesService statFile', () => {
  it('returns size and last-modified from HeadObject', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockResolvedValue({ ContentLength: 42, LastModified: new Date('2026-02-01') })

    await expect(service.statFile('volume-1', 'a.txt')).resolves.toEqual({
      path: 'a.txt',
      size: 42,
      lastModified: new Date('2026-02-01'),
    })
  })

  it('translates a missing object into NotFoundException', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockRejectedValue(notFoundError())

    await expect(service.statFile('volume-1', 'missing.txt')).rejects.toBeInstanceOf(NotFoundException)
  })
})

describe('VolumeFilesService presignRead', () => {
  it('checks existence before signing, and returns the signed URL', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockResolvedValue({ ContentLength: 1 }) // HeadObject success
    ;(getSignedUrl as jest.Mock).mockResolvedValue('https://s3.example/signed-get')

    const result = await service.presignRead('volume-1', 'a.txt')

    expect(result.url).toBe('https://s3.example/signed-get')
    expect(result.expiresAt).toBeInstanceOf(Date)
  })

  it('404s without ever calling getSignedUrl when the object is missing', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockRejectedValue(notFoundError())

    await expect(service.presignRead('volume-1', 'missing.txt')).rejects.toBeInstanceOf(NotFoundException)
    expect(getSignedUrl).not.toHaveBeenCalled()
  })
})

describe('VolumeFilesService presignWrite', () => {
  it('signs a PutObject without checking existence', async () => {
    const { service, send } = createService(READY_VOLUME)
    ;(getSignedUrl as jest.Mock).mockResolvedValue('https://s3.example/signed-put')

    const result = await service.presignWrite('volume-1', 'new-file.txt')

    expect(result.url).toBe('https://s3.example/signed-put')
    expect(send).not.toHaveBeenCalled() // no HeadObject round trip for writes
  })
})

describe('VolumeFilesService deleteFile', () => {
  it('deletes an existing file', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockResolvedValueOnce({ ContentLength: 1 }).mockResolvedValueOnce({})

    await expect(service.deleteFile('volume-1', 'a.txt')).resolves.toBeUndefined()
    expect(send).toHaveBeenCalledTimes(2) // HeadObject then DeleteObject
  })

  it('404s on a path that never existed instead of silently succeeding', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockRejectedValue(notFoundError())

    await expect(service.deleteFile('volume-1', 'missing.txt')).rejects.toBeInstanceOf(NotFoundException)
  })
})

describe('VolumeFilesService batchDelete', () => {
  it('reports partial success from a single DeleteObjects call', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockResolvedValue({
      Deleted: [{ Key: 'a.txt' }],
      Errors: [{ Key: 'b.txt', Message: 'Access Denied' }],
    })

    const result = await service.batchDelete('volume-1', ['a.txt', 'b.txt'])

    expect(result).toEqual({ deleted: ['a.txt'], errors: [{ path: 'b.txt', message: 'Access Denied' }] })
    expect(send).toHaveBeenCalledTimes(1)
  })

  it('chunks more than 1000 keys into multiple DeleteObjects calls', async () => {
    const { service, send } = createService(READY_VOLUME)
    send.mockResolvedValue({ Deleted: [], Errors: [] })
    const paths = Array.from({ length: 1500 }, (_, i) => `file-${i}.txt`)

    await service.batchDelete('volume-1', paths)

    expect(send).toHaveBeenCalledTimes(2)
    expect(send.mock.calls[0][0].input.Delete.Objects).toHaveLength(1000)
    expect(send.mock.calls[1][0].input.Delete.Objects).toHaveLength(500)
  })
})

describe('VolumeFilesService presignBatchWrite', () => {
  it('signs each path independently and reports partial failure', async () => {
    const { service } = createService(READY_VOLUME)
    ;(getSignedUrl as jest.Mock)
      .mockResolvedValueOnce('https://s3.example/a')
      .mockRejectedValueOnce(new Error('signing failed'))

    const result = await service.presignBatchWrite('volume-1', ['a.txt', 'b.txt'])

    expect(result.urls).toEqual([{ path: 'a.txt', url: 'https://s3.example/a', expiresAt: expect.any(Date) }])
    expect(result.errors).toEqual([{ path: 'b.txt', message: 'signing failed' }])
  })

  it('signs in bounded chunks instead of firing every path at once', async () => {
    const { service } = createService(READY_VOLUME)
    let inFlight = 0
    let maxInFlight = 0
    ;(getSignedUrl as jest.Mock).mockImplementation(async () => {
      inFlight++
      maxInFlight = Math.max(maxInFlight, inFlight)
      await Promise.resolve()
      inFlight--
      return 'https://s3.example/signed'
    })
    const paths = Array.from({ length: 120 }, (_, i) => `file-${i}.txt`)

    const result = await service.presignBatchWrite('volume-1', paths)

    expect(result.urls).toHaveLength(120)
    expect(maxInFlight).toBeLessThanOrEqual(50)
  })
})
