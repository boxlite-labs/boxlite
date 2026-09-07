import assert from 'node:assert/strict'
import { createCipheriv, createDecipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { set } from '../src/cli/handlers/env.ts'
import { digestOf } from '../src/env/digest.ts'

const KEY = randomBytes(32)

const sealed = (value: unknown): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', KEY, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

const opened = (payload: Buffer): Record<string, string> => {
  const nonce = payload.subarray(0, 12)
  const decipher = createDecipheriv('aes-256-gcm', KEY, nonce)
  decipher.setAuthTag(payload.subarray(payload.length - 16))
  const body = payload.subarray(12, payload.length - 16)
  return JSON.parse(Buffer.concat([decipher.update(body), decipher.final()]).toString('utf8'))
}

const notFound = (name: string) => Object.assign(new Error(name), { name })

const harness = (stored: Record<string, string>, envDigest: unknown) => {
  const puts: any[] = []
  const store: Record<string, Buffer> = { 'secret/a/dev.json': sealed(stored) }
  const lines: string[] = []
  return {
    puts,
    lines,
    written: () => (puts.length > 0 ? opened(puts[puts.length - 1].Body) : null),
    run: (
      positionals: string[],
      options: Record<string, string | boolean> = {},
      readBatch: () => Promise<string> = async () => '',
    ) =>
      set({
        config: {
          path: '/repo/mstage.config.json',
          envSelectGroup: { deploy: ['A', 'B', 'D'] },
          envDigest,
        } as any,
        scope: { app: 'a', stage: 'dev', protect: false } as any,
        positionals,
        options,
        log: (line: string) => lines.push(line),
        backend: {
          s3: {
            send: async (command: any) => {
              if (command.input.Body) {
                puts.push(command.input)
                return {}
              }
              const object = store[command.input.Key]
              if (!object) throw notFound('NoSuchKey')
              return { Body: { transformToByteArray: async () => object } }
            },
          },
          ssm: {
            send: async (command: any) =>
              command.input.Name === '/sst/bootstrap'
                ? { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
                : { Parameter: { Value: KEY.toString('base64') } },
          },
        } as any,
        readInput: async () => {
          throw new Error('stdin must not be read')
        },
        readBatch,
      }),
  }
}

const DIGEST = { key: 'D', group: 'deploy' }

test('--digest writes the fingerprint of the group as it will be, not as it was', async () => {
  const probe = harness({ A: '0', B: '2', D: 'stale' }, DIGEST)
  await probe.run(['A=1'], { digest: true })
  const written = probe.written()!
  assert.equal(written.A, '1')
  assert.equal(written.D, digestOf({ A: '1', B: '2' }), 'over the new value, and never over D itself')
})

test('the fingerprint can be written on a store that has never held one', async () => {
  // The digest is a member of its own group, so requiring the group to be
  // complete before deriving it demanded the key this write produces.
  const probe = harness({ A: '0', B: '2' }, DIGEST)
  await probe.run(['A=1'], { digest: true })
  assert.equal(probe.written()!.D, digestOf({ A: '1', B: '2' }))
})

test('--digest alone also writes a fingerprint that was never there', async () => {
  const probe = harness({ A: '1', B: '2' }, DIGEST)
  await probe.run([], { digest: true })
  const written = probe.written()!
  assert.equal(written.D, digestOf({ A: '1', B: '2' }))
  assert.deepEqual({ A: written.A, B: written.B }, { A: '1', B: '2' }, 'nothing else moves')
})

test('a group member that is genuinely missing still refuses the write', async () => {
  // Only the digest key is excused; a short group would otherwise be certified
  // as if it were whole.
  const probe = harness({ A: '1' }, DIGEST)
  await assert.rejects(
    () => probe.run(['A=2'], { digest: true }),
    /the store is missing B, which env\.selectGroup\.deploy names/,
  )
  assert.equal(probe.puts.length, 0)
})

test('the assignment and the fingerprint land in one write', async () => {
  const probe = harness({ A: '0', B: '2', D: 'stale' }, DIGEST)
  await probe.run(['A=1'], { digest: true })
  assert.equal(probe.puts.length, 1, 'two writes would leave a moment where they disagree')
})

test('without --digest the fingerprint is left alone, however stale', async () => {
  const probe = harness({ A: '0', B: '2', D: 'stale' }, DIGEST)
  await probe.run(['A=1'])
  assert.equal(probe.written()!.D, 'stale')
})

test('--digest with no env.digest still writes the assignments, and says why not', async () => {
  const probe = harness({ A: '0', B: '2' }, null)
  await probe.run(['A=1'], { digest: true })
  assert.equal(probe.written()!.A, '1', 'the assignment is the part that was asked for')
  assert.equal(probe.written()!.D, undefined, 'and no key is invented to hold a digest')
  assert.match(probe.lines.join('\n'), /declares no env\.digest; nothing to recompute/)
})

test('several assignments are covered by the one fingerprint', async () => {
  const probe = harness({ A: '0', B: '0', D: 'stale' }, DIGEST)
  await probe.run(['A=1', 'B=2'], { digest: true })
  assert.equal(probe.written()!.D, digestOf({ A: '1', B: '2' }))
})

test('a key outside the group changes nothing about the fingerprint', async () => {
  const probe = harness({ A: '1', B: '2', D: 'stale', OUTSIDE: 'x' }, DIGEST)
  await probe.run(['OUTSIDE=y'], { digest: true })
  assert.equal(probe.written()!.D, digestOf({ A: '1', B: '2' }))
})

test('--digest alone recomputes over the store as it stands, writing only the fingerprint', async () => {
  const probe = harness({ A: '1', B: '2', D: 'stale' }, DIGEST)
  await probe.run([], { digest: true })
  const written = probe.written()!
  assert.equal(written.D, digestOf({ A: '1', B: '2' }))
  assert.deepEqual({ A: written.A, B: written.B }, { A: '1', B: '2' }, 'nothing else moves')
})

test('--digest alone on an already-current fingerprint writes nothing', async () => {
  const probe = harness({ A: '1', B: '2', D: digestOf({ A: '1', B: '2' }) }, DIGEST)
  await probe.run([], { digest: true })
  assert.equal(probe.puts.length, 0, 'rewriting identical bytes would only add a version to keep')
})

test('no assignments and no --digest is a usage error, not an empty write', async () => {
  const probe = harness({ A: '1' }, DIGEST)
  await assert.rejects(() => probe.run([]), /usage: npm run mstage env set/)
  assert.equal(probe.puts.length, 0)
})

test('--digest alone never waits on stdin', async () => {
  // A pipe nobody writes to never closes. Reading it to find out whether a
  // batch was meant would hang a CI step whose only job is to recompute.
  const probe = harness({ A: '1', B: '2', D: 'stale' }, DIGEST)
  await probe.run([], { digest: true }, async () => {
    throw new Error('stdin must not be read when --digest is the whole request')
  })
  assert.equal(probe.written()!.D, digestOf({ A: '1', B: '2' }))
})

test('--digest alone without a configured env.digest refuses instead of no-opping', async () => {
  const probe = harness({ A: '1' }, null)
  await assert.rejects(
    () => probe.run([], { digest: true }),
    /declares no env\.digest, so --digest alone would write nothing/,
  )
  assert.equal(probe.puts.length, 0)
})
