/*
 * The GCP backend, against a fake Storage and Secret Manager.
 *
 * The bucket lookup is what most of this checks. It exists so that moving the
 * store is one edit to one record, and a lookup that quietly falls back to a
 * hard-coded name would give that up without anyone noticing — the store would
 * keep working, against the wrong bucket.
 */

import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import test from 'node:test'
import { EnvError, objectKey, seal } from '../src/env/backend.ts'
import { gcpBackend, readStateBucket, type GcpClients } from '../src/env/gcp-backend.ts'
import { readEnvironment, setValues } from '../src/env/store.ts'

const KEY = randomBytes(32)
const PROJECT = 'boxlite-dev'
const BUCKET = 'mstage-state-boxlite-dev'

const notFound = (code: number) => Object.assign(new Error('not found'), { code })

const google = ({
  bootstrap = JSON.stringify({ state: BUCKET }),
  passphrase = KEY.toString('base64'),
  objects = new Map<string, { data: Buffer; generation: number }[]>(),
}: {
  bootstrap?: string | Error
  passphrase?: string | Error
  objects?: Map<string, { data: Buffer; generation: number }[]>
} = {}) => {
  const reads: string[] = []
  let generation = 0
  const clients: GcpClients = {
    storage: {
      bucket: (name) => {
        reads.push(`bucket:${name}`)
        return {
          file: (path, options) => ({
            async download() {
              reads.push(`get:${name}/${path}${options?.generation ? `@${options.generation}` : ''}`)
              const history = objects.get(path) ?? []
              if (history.length === 0) throw notFound(404)
              if (options?.generation === undefined) return [history.at(-1)!.data] as [Buffer]
              const found = history.find((entry) => entry.generation === options.generation)
              if (!found) throw notFound(404)
              return [found.data] as [Buffer]
            },
            async save(data) {
              reads.push(`put:${name}/${path}`)
              const history = objects.get(path) ?? []
              history.push({ data, generation: ++generation })
              objects.set(path, history)
            },
            async getMetadata() {
              const history = objects.get(path) ?? []
              if (history.length === 0) throw notFound(404)
              return [{ generation: history.at(-1)!.generation }] as [{ generation: number }]
            },
            async delete() {
              reads.push(`delete:${name}/${path}`)
              if (!objects.delete(path)) throw notFound(404)
            },
          }),
          // A real prefix match rather than an exact key. `versions` asks with
          // the whole key and gets the same answer either way; listing the lock
          // directory only works if this is what GCS does.
          async getFiles({ prefix }) {
            return [
              [...objects.entries()]
                .filter(([path]) => path.startsWith(prefix))
                .flatMap(([path, history]) =>
                  history.map((entry) => ({
                    name: path,
                    metadata: {
                      generation: entry.generation,
                      size: entry.data.length,
                      updated: '2026-01-01T00:00:00Z',
                    },
                  })),
                ),
            ] as [{ name: string; metadata: Record<string, unknown> }[]]
          },
        }
      },
    },
    secrets: {
      async accessSecretVersion({ name }) {
        reads.push(`secret:${name}`)
        const value = name.includes('mstage-bootstrap') ? bootstrap : passphrase
        if (value instanceof Error) throw value
        return [{ payload: { data: value } }] as [{ payload: { data: string } }]
      },
    },
  }
  return { clients, reads, objects, backend: gcpBackend({ clients, project: PROJECT }) }
}

const store = { app: 'a', stage: 'dev' }

test('the bucket is looked up, never assumed', async () => {
  const probe = google()
  assert.equal(await readStateBucket(probe.clients, PROJECT), BUCKET)
  assert.deepEqual(probe.reads, [`secret:projects/${PROJECT}/secrets/mstage-bootstrap/versions/latest`])
})

test('pointing the record at another bucket moves the store, and nothing else changes', async () => {
  // The whole reason for the lookup: one edit, in one place.
  const moved = google({ bootstrap: JSON.stringify({ state: 'somewhere-else' }) })
  await setValues({ clients: moved.backend, ...store, entries: [['A', '1']] })
  assert.ok(moved.reads.includes('bucket:somewhere-else'))
  assert.equal(moved.reads.some((read) => read.startsWith(`bucket:${BUCKET}`)), false)
})

test('a bootstrap record that names no bucket is refused', async () => {
  await assert.rejects(
    () => readStateBucket(google({ bootstrap: JSON.stringify({ version: 1 }) }).clients, PROJECT),
    /names no state bucket/,
  )
  await assert.rejects(() => readStateBucket(google({ bootstrap: 'not json' }).clients, PROJECT), /not valid JSON/)
  await assert.rejects(() => readStateBucket(google({ bootstrap: notFound(5) }).clients, PROJECT), /does not exist/)
})

test('a written stage reads back through the lookup', async () => {
  const probe = google()
  await setValues({ clients: probe.backend, ...store, entries: [['SMTP_USER', 'alice']] })
  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store }), { SMTP_USER: 'alice' })
  assert.ok(probe.reads.includes(`put:${BUCKET}/${objectKey('a', 'dev')}`), probe.reads.join(' '))
})

test('a stage nobody wrote is empty; a pinned generation that is gone is not', async () => {
  const probe = google()
  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store }), {})
  await assert.rejects(
    () => readEnvironment({ clients: probe.backend, ...store, versionId: '999' }),
    /has no version 999/,
  )
})

test('a version that is not a generation is refused before any call', async () => {
  // GCS generations are integers; an S3-shaped version id would otherwise be
  // sent as NaN and answered with the current object.
  const probe = google()
  await assert.rejects(
    () => readEnvironment({ clients: probe.backend, ...store, versionId: 'hb86RRZQ1eeCnZ' }),
    /is not a GCS generation/,
  )
  assert.equal(probe.reads.some((read) => read.startsWith('get:')), false)
})

test('a pinned generation reads what it named', async () => {
  const probe = google()
  await setValues({ clients: probe.backend, ...store, entries: [['A', 'first']] })
  const pinned = await probe.backend.currentVersion(store)
  await setValues({ clients: probe.backend, ...store, entries: [['A', 'second']] })

  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store }), { A: 'second' })
  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store, versionId: pinned! }), { A: 'first' })
})

test('a missing passphrase says which secret, not which bucket', async () => {
  const probe = google({ passphrase: notFound(5) })
  await probe.backend.write({ ...store, sealed: seal('{"A":"1"}', KEY, 'x') })
  await assert.rejects(() => readEnvironment({ clients: probe.backend, ...store }), EnvError)
  await assert.rejects(
    () => readEnvironment({ clients: probe.backend, ...store }),
    /mstage-passphrase-a-dev\/versions\/latest does not exist/,
  )
})

test('an object sealed on AWS opens here, because the key format is shared', async () => {
  // Moving a stage between clouds is a copy of bytes. The AWS backend seals
  // with the same passphrase encoding and the same nonce ‖ ciphertext ‖ tag.
  const probe = google()
  await probe.backend.write({ ...store, sealed: seal('{"A":"1"}', KEY, objectKey('a', 'dev')) })
  assert.deepEqual(await readEnvironment({ clients: probe.backend, ...store }), { A: '1' })
})

// ── the deployment state Pulumi leaves here ─────────────────────────────────

/*
 * Both keys spelled out, because the two are asymmetric and the asymmetry is
 * the part that gets "corrected" away. Locks carry an `organization/` segment
 * and stacks do not: locks are keyed by `FullyQualifiedName()`, which renders a
 * project-scoped reference as `organization/<project>/<stack>`, while stacks go
 * through the reference store, which joins only the project and the name.
 * Pulumi's published documentation describes the lock path without the segment.
 */
const APP = 'boxlite-backoffice'
const CHECKPOINT = `.pulumi/stacks/${APP}/dev.json`
const LOCKS = `.pulumi/locks/organization/${APP}/dev/`
const deploying = { app: APP, stage: 'dev' }

/** Puts one object in the bucket at an exact key, the way Pulumi would. */
const written = (objects: Map<string, { data: Buffer; generation: number }[]>, key: string, body: string) =>
  objects.set(key, [{ data: Buffer.from(body), generation: 1 }])

test('a GCP home keeps deployment state, because Pulumi puts it in this bucket', () => {
  // It used to answer null here, on the reasoning that nothing deploys into a
  // GCP home. Since mdeploy grew its Pulumi engine that is simply untrue: the
  // engine writes its checkpoint and takes its lock in the same bucket mstage
  // keeps the store in, so a stopped deploy leaves both behind.
  assert.notEqual(google().backend.state, null)
})

test("the checkpoint is Pulumi's own key, not the one SST writes", async () => {
  const objects = new Map<string, { data: Buffer; generation: number }[]>()
  written(objects, CHECKPOINT, '{"version":3,"checkpoint":{}}')
  const probe = google({ objects })
  const payload = await probe.backend.state!.readCheckpoint(deploying)
  assert.equal(payload?.toString(), '{"version":3,"checkpoint":{}}')
  // SST's layout is `app/<app>/<stage>.json`; reading that here would find
  // nothing and report a stage nothing had deployed into.
  assert.ok(!probe.reads.some((read) => read.includes('app/boxlite-backoffice/dev.json')), probe.reads.join(' '))
})

test('a written checkpoint lands where the engine will look for it', async () => {
  const probe = google()
  await probe.backend.state!.writeCheckpoint({ ...deploying, checkpoint: Buffer.from('{"version":3}') })
  assert.deepEqual([...probe.objects.keys()], [CHECKPOINT])
})

test('a lock is a file in a directory, because that is how Pulumi holds one', async () => {
  const objects = new Map<string, { data: Buffer; generation: number }[]>()
  written(objects, `${LOCKS}b7a1f2.json`, '{"pid":41,"hostname":"runner"}')
  const held = await google({ objects }).backend.state!.readLock(deploying)
  assert.equal(held?.toString(), '{"pid":41,"hostname":"runner"}')
})

test('an empty lock directory is no lock, which is an answer rather than a failure', async () => {
  assert.equal(await google().backend.state!.readLock(deploying), null)
})

test('the lock directory is the one the engine writes, organization segment and all', async () => {
  // The failure this guards is silent and the wrong way round: a prefix missing
  // that segment lists nothing, so a held stage reports free — `unlock` says
  // there is nothing to drop and `edit` opens a checkpoint a live deploy owns.
  const objects = new Map<string, { data: Buffer; generation: number }[]>()
  written(objects, `.pulumi/locks/${APP}/dev/b7a1f2.json`, '{"pid":41}')
  assert.equal(await google({ objects }).backend.state!.readLock(deploying), null, 'read a path Pulumi never writes')

  written(objects, `${LOCKS}b7a1f2.json`, '{"pid":41}')
  assert.notEqual(await google({ objects }).backend.state!.readLock(deploying), null)
})

test('two locks are refused rather than one of them reported', async () => {
  // Pulumi writes one file per operation holding the stage, so two means two
  // holders. Picking one would name a holder the caller did not ask about and
  // would make `clearLock`'s "the lock I named is the lock I dropped" a lie.
  const objects = new Map<string, { data: Buffer; generation: number }[]>()
  written(objects, `${LOCKS}b7a1f2.json`, '{"pid":41}')
  written(objects, `${LOCKS}c9e4d8.json`, '{"pid":57}')
  await assert.rejects(() => google({ objects }).backend.state!.readLock(deploying), /2 locks/)
})

test('dropping the lock removes every file the engine left under the stage', async () => {
  // Two, because one proves nothing: an implementation that deleted only the
  // first would pass with a single file and leave the stage locked, which is
  // exactly the outcome removeLock exists to prevent.
  const objects = new Map<string, { data: Buffer; generation: number }[]>()
  written(objects, `${LOCKS}b7a1f2.json`, '{"pid":41}')
  written(objects, `${LOCKS}c9e4d8.json`, '{"pid":57}')
  written(objects, CHECKPOINT, '{"version":3}')
  const probe = google({ objects })
  await probe.backend.state!.removeLock(deploying)
  // The checkpoint is not a lock and must survive: dropping a lock is what a
  // stage needs before it can be deployed again, not a reset.
  assert.deepEqual([...probe.objects.keys()], [CHECKPOINT])
})

test('dropping a lock nobody holds is not a failure', async () => {
  await google().backend.state!.removeLock(deploying)
})
