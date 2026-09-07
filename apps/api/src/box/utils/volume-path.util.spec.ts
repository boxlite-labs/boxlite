import { assertSafeVolumePath } from './volume-path.util'

describe('assertSafeVolumePath', () => {
  it.each(['checkpoints/model.bin', 'config.json', 'a/b/c.txt'])('accepts a relative path %s', (input) => {
    expect(() => assertSafeVolumePath(input)).not.toThrow()
  })

  it.each(['/config.json', '/checkpoints/model.bin'])('rejects an absolute path %s', (input) => {
    expect(() => assertSafeVolumePath(input)).toThrow('escapes the volume root')
  })

  it.each(['..', '../secret', 'checkpoints/../../secret', '../../etc/passwd'])(
    'rejects a path that escapes upward %s',
    (input) => {
      expect(() => assertSafeVolumePath(input)).toThrow('escapes the volume root')
    },
  )

  it('rejects an empty path', () => {
    expect(() => assertSafeVolumePath('')).toThrow('must not be empty')
  })

  it('normalizes a redundant relative segment without rejecting it', () => {
    expect(() => assertSafeVolumePath('checkpoints/./model.bin')).not.toThrow()
  })
})
