import assert from 'node:assert/strict'
import { createCipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { currentVersion, readEnvironment, type Clients } from '../src/env/store.ts'

const KEY = randomBytes(32)

const sealed = (value: unknown): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', KEY, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

const notFound = (name: string) => Object.assign(new Error(name), { name })

/** Keyed by version id; `null` stands for the current one, as S3 reports it. */
const clients = (versions: Record<string, unknown>, head: unknown = { VersionId: 'v2' }) => {
  const asked: Record<string, unknown>[] = []
  return {
    asked,
    clients: {
      s3: {
        send: async (command: any) => {
          asked.push(command.input)
          if (command.constructor.name === 'HeadObjectCommand') {
            if (head instanceof Error) throw head
            return head
          }
          const wanted = command.input.VersionId ?? 'current'
          const object = versions[wanted]
          if (object === undefined) throw notFound('NoSuchKey')
          return { Body: { transformToByteArray: async () => sealed(object) } }
        },
      },
      ssm: {
        send: async (command: any) =>
          command.input.Name === '/sst/bootstrap'
            ? { Parameter: { Value: JSON.stringify({ state: 'sst-state-abcdefghijkl' }) } }
            : { Parameter: { Value: KEY.toString('base64') } },
      },
    } as unknown as Clients,
  }
}

test('without a version the store answers with what it holds now', async () => {
  const probe = clients({ current: { A: 'now' }, v1: { A: 'then' } })
  assert.deepEqual(await readEnvironment({ clients: probe.clients, app: 'a', stage: 'dev' }), { A: 'now' })
  assert.equal(probe.asked.at(-1)?.VersionId, undefined, 'no VersionId is sent, so S3 picks the current one')
})

test('a pinned version reads what the deploy saw, not what the store holds now', async () => {
  const probe = clients({ current: { A: 'edited-since' }, v1: { A: 'at-deploy' } })
  const values = await readEnvironment({ clients: probe.clients, app: 'a', stage: 'dev', versionId: 'v1' })
  assert.deepEqual(values, { A: 'at-deploy' })
  assert.equal(probe.asked.at(-1)?.VersionId, 'v1')
})

test('a version that no longer exists fails rather than falling back to current', async () => {
  // An expired version is a real risk once a lifecycle rule touches the bucket,
  // and quietly reading current would be the drift the pin exists to prevent.
  const probe = clients({ current: { A: 'now' } })
  await assert.rejects(
    () => readEnvironment({ clients: probe.clients, app: 'a', stage: 'dev', versionId: 'gone' }),
    /has no version gone; it was deleted or expired/,
  )
})

test('currentVersion reports the id a deploy should record', async () => {
  const probe = clients({ current: { A: '1' } }, { VersionId: 'v7' })
  assert.equal(await currentVersion({ clients: probe.clients, app: 'a', stage: 'dev' }), 'v7')
})

test('an unversioned bucket and an unwritten stage both pin nothing', async () => {
  assert.equal(
    await currentVersion({ clients: clients({}, { VersionId: 'null' }).clients, app: 'a', stage: 'dev' }),
    null,
    'S3 reports the literal "null" for an object in a bucket without versioning',
  )
  assert.equal(
    await currentVersion({ clients: clients({}, notFound('NotFound')).clients, app: 'a', stage: 'dev' }),
    null,
  )
})
