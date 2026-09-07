/*
 * `mstage state unlock`, against the keys SST actually writes.
 *
 * The fake starts at the S3 and SSM calls rather than at the backend, because
 * the whole job of this command is to remove one specific object: a fake that
 * began above the layout would let it delete the wrong key and still pass.
 */

import assert from 'node:assert/strict'
import test from 'node:test'
import { unlock } from '../src/cli/handlers/state.ts'
import { awsBackend } from '../src/env/aws-backend.ts'
import type { StoreBackend } from '../src/env/backend.ts'

const LOCK_KEY = 'lock/boxlite/dev.json'
const CHECKPOINT_KEY = 'app/boxlite/dev.json'
const STATE_BUCKET = 'sst-state-x'

/** What SST records while a deploy holds the lock (`lockData`). */
const HELD = JSON.stringify({
  created: '2026-09-04T09:12:03.113Z',
  updateID: '01K4A6EX7M',
  runID: '17512345678',
  command: 'deploy',
  ignore: false,
})

const notFound = (name: string) => Object.assign(new Error(name), { name })

/**
 * `meanwhile` runs after the first read of the lock, which is where a deploy
 * that takes the lock between naming it and dropping it gets to happen.
 */
const bucket = ({ lock, meanwhile }: { lock?: string; meanwhile?: () => void } = {}) => {
  const objects = new Map<string, Buffer>([[CHECKPOINT_KEY, Buffer.from('{"version":3,"checkpoint":{}}')]])
  if (lock !== undefined) objects.set(LOCK_KEY, Buffer.from(lock))
  const deleted: string[] = []
  let reads = 0

  const clients = {
    ssm: {
      send: async (command: any) => {
        if (command.input.Name === '/sst/bootstrap') {
          return { Parameter: { Value: JSON.stringify({ state: STATE_BUCKET }) } }
        }
        throw notFound('ParameterNotFound')
      },
    },
    s3: {
      send: async (command: any) => {
        const key: string = command.input.Key
        if (command.constructor.name === 'DeleteObjectCommand') {
          deleted.push(key)
          objects.delete(key)
          return {}
        }
        const object = objects.get(key)
        if (key === LOCK_KEY && ++reads === 1) meanwhile?.()
        if (!object) throw notFound('NoSuchKey')
        return { Body: { transformToByteArray: async () => object } }
      },
    },
  }
  return { objects, deleted, backend: awsBackend(clients) as StoreBackend }
}

const drop = ({
  options = {},
  protect = false,
  backend,
}: {
  options?: Record<string, string | boolean>
  protect?: boolean
  backend: StoreBackend
}) => {
  const lines: string[] = []
  return {
    lines,
    run: () =>
      unlock({
        scope: { app: 'boxlite', stage: 'dev', protect } as any,
        options,
        log: (line: string) => lines.push(line),
        backend,
      }),
  }
}

test('the object removed is the lock SST writes, and only that one', async () => {
  const store = bucket({ lock: HELD })
  const attempt = drop({ backend: store.backend })
  assert.equal(await attempt.run(), 0)
  assert.deepEqual(store.deleted, [LOCK_KEY])
  assert.ok(store.objects.has(CHECKPOINT_KEY), 'the checkpoint is not what a lock is')
})

test('what held the lock is named before it goes', async () => {
  // The one thing this cannot tell is whether that deploy is still running, so
  // the run and the time are what let the operator answer it.
  const attempt = drop({ backend: bucket({ lock: HELD }).backend })
  assert.equal(await attempt.run(), 0)
  const printed = attempt.lines.join('\n')
  assert.match(printed, /boxlite\/dev is locked by deploy in run 17512345678, update 01K4A6EX7M, since 2026-09-04/)
  assert.match(printed, /^lock removed from boxlite\/dev$/m)
})

test('a stage with no lock is an answer, not a failure', async () => {
  const store = bucket()
  const attempt = drop({ backend: store.backend })
  assert.equal(await attempt.run(), 0)
  assert.deepEqual(attempt.lines, ['# boxlite/dev holds no lock'])
  assert.deepEqual(store.deleted, [], 'nothing to remove means nothing is sent')
})

test('a lock nobody can parse is still removed, because that is the point of the command', async () => {
  const store = bucket({ lock: 'truncated by whatever died holding it' })
  const attempt = drop({ backend: store.backend })
  assert.equal(await attempt.run(), 0)
  assert.deepEqual(store.deleted, [LOCK_KEY])
  assert.match(attempt.lines.join('\n'), /locked by an unrecorded command/)
})

test('a lock taken between naming one and dropping it is not the one removed', async () => {
  // Otherwise the operator is told about last week's lock while a live deploy
  // loses the one it is holding.
  const taken = JSON.stringify({ command: 'deploy', runID: '999', updateID: '01NEW' })
  const store = bucket({ lock: HELD, meanwhile: () => store.objects.set(LOCK_KEY, Buffer.from(taken)) })
  const attempt = drop({ backend: store.backend })
  await assert.rejects(() => attempt.run(), /is not the lock that was just reported/)
  assert.deepEqual(store.deleted, [])
  assert.match(attempt.lines.join('\n'), /locked by deploy in run 17512345678/)
})

test('a lock that goes on its own leaves the stage as the caller asked for it', async () => {
  const store = bucket({ lock: HELD, meanwhile: () => store.objects.delete(LOCK_KEY) })
  const attempt = drop({ backend: store.backend })
  assert.equal(await attempt.run(), 0)
  assert.deepEqual(store.deleted, [], 'nothing is sent for an object that is already gone')
  assert.match(attempt.lines.join('\n'), /# that lock went before it could be removed/)
})

test('a protected stage keeps its lock until --confirm says otherwise', async () => {
  const store = bucket({ lock: HELD })
  await assert.rejects(() => drop({ backend: store.backend, protect: true }).run(), /add --confirm to drop its lock/)
  assert.deepEqual(store.deleted, [])

  const confirmed = drop({ backend: store.backend, protect: true, options: { confirm: true } })
  assert.equal(await confirmed.run(), 0)
  assert.deepEqual(store.deleted, [LOCK_KEY])
})

test("a Pulumi lock is described by what it records, not by what SST would have", async () => {
  // `lockContent` (pkg/backend/diy/lock.go) records the person and the machine
  // where SST records the command and the run. A renderer that read only SST's
  // fields answered "an unrecorded command" and dropped all four, on the one
  // cloud where the operator has least else to go on.
  const held = JSON.stringify({
    pid: 41,
    username: 'kx',
    hostname: 'runner-1',
    timestamp: '2026-09-06T11:02:44.117Z',
  })
  const attempt = drop({ backend: bucket({ lock: held }).backend })
  assert.equal(await attempt.run(), 0)
  assert.match(attempt.lines.join('\n'), /locked by kx on runner-1, pid 41, since 2026-09-06T11:02:44\.117Z/)
})

test('a lock swapped for another the reader cannot parse is still not the one removed', async () => {
  // The guard compares what was named against what is there. It used to compare
  // only the four fields SST records, so two locks written by an engine that
  // records different ones — Pulumi's, on a GCP stage — both read as four
  // nulls, compared equal, and the second was deleted while the first was the
  // one reported. Identity has to come from the bytes, not from fields this
  // reader happens to recognise.
  const foreign = JSON.stringify({ pid: 41, hostname: 'runner-a' })
  const swapped = JSON.stringify({ pid: 57, hostname: 'runner-b' })
  const store = bucket({ lock: foreign, meanwhile: () => store.objects.set(LOCK_KEY, Buffer.from(swapped)) })
  const attempt = drop({ backend: store.backend })
  await assert.rejects(() => attempt.run(), /is not the lock that was just reported/)
  assert.deepEqual(store.deleted, [], 'a lock nobody named was removed')
})

test('the state bucket is not named, on either outcome', async () => {
  for (const lock of [HELD, undefined]) {
    const attempt = drop({ backend: bucket({ lock }).backend })
    assert.equal(await attempt.run(), 0)
    assert.ok(!attempt.lines.join('\n').includes(STATE_BUCKET), attempt.lines.join('\n'))
  }
})
