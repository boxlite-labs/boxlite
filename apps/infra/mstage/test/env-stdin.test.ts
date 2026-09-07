import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import { Readable } from 'node:stream'
import test from 'node:test'
import { set } from '../src/cli/handlers/env.ts'
import { readValue } from '../src/cli/prompt.ts'

const PEM = ['-----BEGIN RSA PRIVATE KEY-----', 'MIIEpAIBAAKCAQEA', '-----END RSA PRIVATE KEY-----', ''].join('\n')

const scope = { app: 'boxlite', stage: 'dev', protect: false } as any

/** Captures what reached the store without going near S3. */
const capture = () => {
  const written: { name: string; value: string }[] = []
  const lines: string[] = []
  return {
    written,
    lines,
    backend: {} as any,
    set: (positionals: string[], readInput?: () => Promise<string>) =>
      set({
        config: { path: '/repo/mstage.config.json', envSelectGroup: {}, envDigest: null } as any,
        scope,
        positionals,
        options: {},
        log: (line: string) => lines.push(line),
        backend: {
          s3: {
            send: async (command: any) => {
              if (command.input.Body) {
                written.push({ name: command.input.Key, value: command.input.Body })
                return {}
              }
              throw Object.assign(new Error('NoSuchKey'), { name: 'NoSuchKey' })
            },
          },
          ssm: {
            send: async (command: any) =>
              command.input.Name === '/sst/bootstrap'
                ? { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
                : { Parameter: { Value: randomBytes(32).toString('base64') } },
          },
        } as any,
        readBatch: async () => '',
        ...(readInput ? { readInput } : {}),
      }),
  }
}

test('a redirect is read whole, newlines and all', async () => {
  const stream = Readable.from([PEM]) as any
  stream.isTTY = false
  assert.equal(await readValue(stream), PEM)
  assert.equal((await readValue(Object.assign(Readable.from([PEM]), { isTTY: false }) as any)).split('\n').length, 4)
})

test("a file's trailing newline is kept, because that is what SST stores", async () => {
  const stream = Object.assign(Readable.from(['value\n']), { isTTY: false }) as any
  assert.equal(await readValue(stream), 'value\n')
})

test('a redirect arriving in several chunks is joined, not truncated', async () => {
  const stream = Object.assign(Readable.from(['-----BEGIN', ' RSA-----\n', 'body\n']), { isTTY: false }) as any
  assert.equal(await readValue(stream), '-----BEGIN RSA-----\nbody\n')
})

test('a name with no value takes it from stdin', async () => {
  const probe = capture()
  let asked = 0
  await probe.set(['PRIVATE_KEY'], async () => {
    asked += 1
    return PEM
  })
  assert.equal(asked, 1, 'a missing value must consult stdin')
  assert.equal(probe.written.length, 1, 'and what it read must be written')
})

test('an assignment with an empty value stores the empty string', async () => {
  // `OPTIONAL=` is an assignment; only a bare name asks for stdin.
  const probe = capture()
  await probe.set(['OPTIONAL='], async () => {
    throw new Error('stdin must not be read when an assignment was written')
  })
  assert.equal(probe.written.length, 1)
})

test('a written value is used as written, and stdin is never touched', async () => {
  const probe = capture()
  await probe.set(['PORT=8080'], async () => {
    throw new Error('stdin must not be read when a value was written')
  })
})

test('a value containing "=" is kept whole, because only the first one splits', async () => {
  const probe = capture()
  await probe.set(['B64=aGVsbG8='], async () => {
    throw new Error('stdin must not be read')
  })
  assert.equal(probe.written.length, 1)
})

test('several assignments are written together, in one object', async () => {
  const probe = capture()
  await probe.set(['A=1', 'B=2'], async () => {
    throw new Error('stdin must not be read')
  })
  assert.equal(probe.written.length, 1, 'one read-modify-write, not one per key')
})

test('the same key twice is refused rather than letting the last one win', async () => {
  const probe = capture()
  await assert.rejects(() => probe.set(['A=1', 'A=2']), /A given more than once/)
})

test('a bare name alongside others is refused: there is only one stdin', async () => {
  const probe = capture()
  await assert.rejects(() => probe.set(['A=1', 'B']), /"B" is not a KEY=VALUE assignment/)
})

test('the confirmation names the stage it wrote to, not the bucket it wrote in', async () => {
  // The state bucket's random suffix is what keeps these objects unaddressable;
  // printing it on every set would put it in every operator's scrollback.
  const probe = capture()
  await probe.set(['PORT=8080'])
  assert.deepEqual(probe.lines, [
    'PORT added in boxlite/dev',
    '# run a deploy for the change to reach anything running',
  ])
})
