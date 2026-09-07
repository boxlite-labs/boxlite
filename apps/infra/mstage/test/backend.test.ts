/*
 * The store, exercised against a backend that is neither cloud.
 *
 * If every test reaches for S3 fakes then the interface is decoration: the code
 * still only works one way and nobody would find out until a second cloud
 * existed. These run the real read-modify-write, sealing, digest-friendly
 * ordering and delete paths against an in-memory backend, which is the check
 * that the AWS layout has actually left this layer.
 */

import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import test from 'node:test'
import { objectKey, open, seal, type StoreBackend, type StoredVersion } from '../src/env/backend.ts'
import { currentVersion, deleteValues, listVersions, readEnvironment, setValues } from '../src/env/store.ts'

const KEY = randomBytes(32)

/** A backend with no cloud behind it: objects in a Map, versions as a counter. */
const memoryBackend = () => {
  const objects = new Map<string, { sealed: Buffer; version: string }[]>()
  const calls: string[] = []
  let counter = 0
  const backend: StoreBackend = {
    home: 'memory',
    async read({ app, stage, versionId }) {
      calls.push(`read ${objectKey(app, stage)}${versionId ? `@${versionId}` : ''}`)
      const history = objects.get(objectKey(app, stage)) ?? []
      if (!versionId) return history.at(-1)?.sealed ?? null
      const found = history.find((entry) => entry.version === versionId)
      if (!found) throw new Error(`${objectKey(app, stage)} has no version ${versionId}`)
      return found.sealed
    },
    async write({ app, stage, sealed }) {
      calls.push(`write ${objectKey(app, stage)}`)
      const history = objects.get(objectKey(app, stage)) ?? []
      history.push({ sealed, version: String(++counter) })
      objects.set(objectKey(app, stage), history)
    },
    async currentVersion({ app, stage }) {
      return objects.get(objectKey(app, stage))?.at(-1)?.version ?? null
    },
    async versions({ app, stage }) {
      return (objects.get(objectKey(app, stage)) ?? [])
        .map(
          (entry): StoredVersion => ({
            versionId: entry.version,
            type: 'version',
            lastModified: null,
            size: entry.sealed.length,
            storageClass: null,
          }),
        )
        .reverse()
    },
    async passphrase() {
      calls.push('passphrase')
      return KEY
    },
    /*
     * Nothing deploys into a Map, and these tests ask the store's five
     * questions rather than the engine's two. Refusing rather than answering
     * emptily: a test that reached one of these by accident would otherwise
     * read a checkpoint that was never written and call it an empty stage.
     */
    state: {
      readCheckpoint: () => Promise.reject(new Error('the in-memory backend keeps no checkpoint')),
      writeCheckpoint: () => Promise.reject(new Error('the in-memory backend keeps no checkpoint')),
      readLock: () => Promise.reject(new Error('the in-memory backend takes no lock')),
      removeLock: () => Promise.reject(new Error('the in-memory backend takes no lock')),
    },
  }
  return { backend, calls, objects }
}

const store = { app: 'a', stage: 'dev' }

test('a stage nobody has written reads as empty, not as a failure', async () => {
  const { backend } = memoryBackend()
  assert.deepEqual(await readEnvironment({ clients: backend, ...store }), {})
})

test('what is written comes back, through seal and open', async () => {
  const probe = memoryBackend()
  await setValues({ clients: probe.backend, ...store, entries: [['SMTP_USER', 'alice']] })
  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store }), { SMTP_USER: 'alice' })
})

test('the backend never holds anything readable', async () => {
  // It stores bytes it cannot interpret. A backend that could read a value
  // would be a second place a secret has to be protected.
  const probe = memoryBackend()
  await setValues({ clients: probe.backend, ...store, entries: [['TOKEN', 'a-secret-value']] })
  const stored = probe.objects.get(objectKey('a', 'dev'))!.at(-1)!.sealed
  assert.equal(stored.includes('a-secret-value'), false, 'the value reached the backend in the clear')
  assert.equal(open(stored, KEY, 'x'), '{"TOKEN":"a-secret-value"}')
})

test('several keys are one read-modify-write, not one round trip each', async () => {
  const probe = memoryBackend()
  await setValues({
    clients: probe.backend,
    ...store,
    entries: [
      ['A', '1'],
      ['B', '2'],
    ],
  })
  assert.equal(probe.calls.filter((call) => call.startsWith('write')).length, 1)
})

test('a derived value lands in the same write as what it describes', async () => {
  // A digest written second would leave a moment where the store disagrees
  // with its own fingerprint.
  const probe = memoryBackend()
  await setValues({
    clients: probe.backend,
    ...store,
    entries: [['A', '1']],
    derive: (values) => [['DIGEST', Object.keys(values).join(',')]],
  })
  assert.equal(probe.calls.filter((call) => call.startsWith('write')).length, 1)
  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store }), { A: '1', DIGEST: 'A' })
})

test('writing the same value again writes nothing', async () => {
  const probe = memoryBackend()
  await setValues({ clients: probe.backend, ...store, entries: [['A', '1']] })
  const before = probe.calls.filter((call) => call.startsWith('write')).length
  const { outcomes } = await setValues({ clients: probe.backend, ...store, entries: [['A', '1']] })
  assert.equal(probe.calls.filter((call) => call.startsWith('write')).length, before)
  assert.deepEqual(outcomes, [{ name: 'A', existed: true, unchanged: true }])
})

test('a version can be pinned and read back on any backend', async () => {
  const probe = memoryBackend()
  await setValues({ clients: probe.backend, ...store, entries: [['A', 'first']] })
  const pinned = await currentVersion({ clients: probe.backend, ...store })
  await setValues({ clients: probe.backend, ...store, entries: [['A', 'second']] })

  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store }), { A: 'second' })
  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store, versionId: pinned! }), { A: 'first' })
  assert.equal((await listVersions({ clients: probe.backend, ...store })).length, 2)
})

test('removing a key that was never there is reported, not raised', async () => {
  const probe = memoryBackend()
  const removal = await deleteValues({ clients: probe.backend, ...store, names: ['ABSENT'] })
  assert.deepEqual(removal.outcomes, [{ name: 'ABSENT', existed: false }])
  assert.equal(probe.calls.filter((call) => call.startsWith('write')).length, 0)
})

test('an object sealed by one backend opens on another', async () => {
  // The point of sealing above the backend: moving a stage between clouds is a
  // copy of bytes, not a re-entry of every value.
  const first = memoryBackend()
  await setValues({ clients: first.backend, ...store, entries: [['A', '1']] })
  const bytes = first.objects.get(objectKey('a', 'dev'))!.at(-1)!.sealed

  const second = memoryBackend()
  await second.backend.write({ ...store, sealed: bytes })
  assert.deepEqual(await readEnvironment({ clients: second.backend, ...store }), { A: '1' })
})

test('a name SST would refuse never reaches a backend', async () => {
  const probe = memoryBackend()
  await assert.rejects(
    () => setValues({ clients: probe.backend, ...store, entries: [['lower_case', 'x']] }),
    /is not a usable name/,
  )
  assert.deepEqual(probe.calls, [])
})

test('sealing is the same on every backend, so the bytes are portable', () => {
  const sealed = seal('{"A":"1"}', KEY, 'x')
  assert.equal(open(sealed, KEY, 'x'), '{"A":"1"}')
  // nonce ‖ ciphertext ‖ tag, as Go's gcm.Seal writes it.
  assert.equal(sealed.length, 12 + '{"A":"1"}'.length + 16)
})
