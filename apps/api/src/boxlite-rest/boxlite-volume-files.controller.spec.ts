import { BoxliteVolumeFilesController } from './boxlite-volume-files.controller'
import { VolumeFilesService } from '../box/services/volume-files.service'

describe('BoxliteVolumeFilesController', () => {
  function createController() {
    const volumeFilesService = {
      listFiles: jest.fn().mockResolvedValue({ entries: [], hasMore: false }),
      statFile: jest.fn().mockResolvedValue({ path: 'a.txt', size: 1, lastModified: new Date() }),
      presignRead: jest.fn().mockResolvedValue({ url: 'https://s3.example/read', expiresAt: new Date() }),
      presignWrite: jest.fn().mockResolvedValue({ url: 'https://s3.example/write', expiresAt: new Date() }),
      deleteFile: jest.fn().mockResolvedValue(undefined),
      batchDelete: jest.fn().mockResolvedValue({ deleted: ['a.txt'], errors: [] }),
      presignBatchWrite: jest.fn().mockResolvedValue({ urls: [], errors: [] }),
    }
    return {
      controller: new BoxliteVolumeFilesController(volumeFilesService as unknown as VolumeFilesService),
      volumeFilesService,
    }
  }

  it('lists files, defaulting to the volume root', async () => {
    const { controller, volumeFilesService } = createController()

    await controller.listFiles('volume-1', undefined as never, 'cursor-1')

    expect(volumeFilesService.listFiles).toHaveBeenCalledWith('volume-1', '', 'cursor-1')
  })

  it('lists files under a given path', async () => {
    const { controller, volumeFilesService } = createController()

    await controller.listFiles('volume-1', 'checkpoints/')

    expect(volumeFilesService.listFiles).toHaveBeenCalledWith('volume-1', 'checkpoints/', undefined)
  })

  it('delegates statFile to the service', async () => {
    const { controller, volumeFilesService } = createController()

    await controller.statFile('volume-1', 'a.txt')

    expect(volumeFilesService.statFile).toHaveBeenCalledWith('volume-1', 'a.txt')
  })

  it('delegates presignRead to the service', async () => {
    const { controller, volumeFilesService } = createController()

    await controller.presignRead('volume-1', 'a.txt')

    expect(volumeFilesService.presignRead).toHaveBeenCalledWith('volume-1', 'a.txt')
  })

  it('delegates presignWrite to the service', async () => {
    const { controller, volumeFilesService } = createController()

    await controller.presignWrite('volume-1', 'a.txt')

    expect(volumeFilesService.presignWrite).toHaveBeenCalledWith('volume-1', 'a.txt')
  })

  it('delegates deleteFile to the service', async () => {
    const { controller, volumeFilesService } = createController()

    await expect(controller.deleteFile('volume-1', 'a.txt')).resolves.toBeUndefined()
    expect(volumeFilesService.deleteFile).toHaveBeenCalledWith('volume-1', 'a.txt')
  })

  it('delegates batchDelete to the service with the DTO paths', async () => {
    const { controller, volumeFilesService } = createController()

    await controller.batchDelete('volume-1', { paths: ['a.txt', 'b.txt'] })

    expect(volumeFilesService.batchDelete).toHaveBeenCalledWith('volume-1', ['a.txt', 'b.txt'])
  })

  it('delegates presignBatchWrite to the service with the DTO paths', async () => {
    const { controller, volumeFilesService } = createController()

    await controller.presignBatchWrite('volume-1', { paths: ['a.txt', 'b.txt'] })

    expect(volumeFilesService.presignBatchWrite).toHaveBeenCalledWith('volume-1', ['a.txt', 'b.txt'])
  })
})
