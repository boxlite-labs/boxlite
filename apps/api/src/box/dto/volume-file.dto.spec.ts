import { validate } from 'class-validator'
import { plainToInstance } from 'class-transformer'
import { BatchDeleteVolumeFilesDto, PresignBatchWriteVolumeFilesDto } from './volume-file.dto'

describe.each([
  ['BatchDeleteVolumeFilesDto', BatchDeleteVolumeFilesDto],
  ['PresignBatchWriteVolumeFilesDto', PresignBatchWriteVolumeFilesDto],
])('%s.paths validation', (_name, DtoClass) => {
  it('accepts a non-empty array within the size cap', async () => {
    const dto = plainToInstance(DtoClass, { paths: ['a.txt', 'b.txt'] })
    expect(await validate(dto)).toHaveLength(0)
  })

  it('accepts exactly 1000 paths (the cap boundary)', async () => {
    const dto = plainToInstance(DtoClass, { paths: Array.from({ length: 1000 }, (_, i) => `f${i}.txt`) })
    expect(await validate(dto)).toHaveLength(0)
  })

  it('rejects an empty array', async () => {
    const dto = plainToInstance(DtoClass, { paths: [] })
    const errors = await validate(dto)
    expect(errors.some((e) => e.constraints && 'arrayNotEmpty' in e.constraints)).toBe(true)
  })

  it('rejects more than 1000 paths', async () => {
    const dto = plainToInstance(DtoClass, { paths: Array.from({ length: 1001 }, (_, i) => `f${i}.txt`) })
    const errors = await validate(dto)
    expect(errors.some((e) => e.constraints && 'arrayMaxSize' in e.constraints)).toBe(true)
  })
})
