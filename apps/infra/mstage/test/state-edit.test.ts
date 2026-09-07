/*
 * `mstage state edit`, from the object it opens to the object it writes back.
 *
 * The editor is a fake process that saves a chosen file and exits, which is what
 * makes the interesting half testable: what is handed to it, what is done with
 * what comes back, and what happens to the copy when the write is refused.
 */

import assert from 'node:assert/strict'
import { existsSync } from 'node:fs'
import { readFile, rm } from 'node:fs/promises'
import test from 'node:test'
import { edit, editorCommand } from '../src/cli/handlers/state.ts'
import { awsBackend } from '../src/env/aws-backend.ts'
import type { StoreBackend } from '../src/env/backend.ts'

const LOCK_KEY = 'lock/boxlite/dev.json'
const CHECKPOINT_KEY = 'app/boxlite/dev.json'
const STATE_BUCKET = 'sst-state-x'

/** A checkpoint a cancelled deploy left behind: one operation still recorded as in flight. */
const STUCK = JSON.stringify({
  version: 3,
  checkpoint: {
    stack: 'organization/boxlite/dev',
    latest: {
      resources: [{ urn: 'urn:pulumi:dev::boxlite::sst:aws:Cluster::Cluster' }],
      pending_operations: [{ type: 'creating', resource: { urn: 'urn:pulumi:dev::boxlite::sst:aws:Service::Api' } }],
    },
  },
})

/** The same checkpoint with the pending operation deleted, which is the edit that unsticks it. */
const REPAIRED = STUCK.replace(
  '"pending_operations":[{"type":"creating","resource":{"urn":"urn:pulumi:dev::boxlite::sst:aws:Service::Api"}}]',
  '"pending_operations":[]',
)

const notFound = (name: string) => Object.assign(new Error(name), { name })

const bucket = ({ checkpoint = STUCK, lock }: { checkpoint?: string | null; lock?: string } = {}) => {
  const objects = new Map<string, Buffer>()
  if (checkpoint !== null) objects.set(CHECKPOINT_KEY, Buffer.from(checkpoint))
  if (lock !== undefined) objects.set(LOCK_KEY, Buffer.from(lock))
  const written: { key: string; body: string }[] = []

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
        if (command.constructor.name === 'PutObjectCommand') {
          const body = Buffer.from(command.input.Body)
          written.push({ key, body: body.toString('utf8') })
          objects.set(key, body)
          return {}
        }
        const object = objects.get(key)
        if (!object) throw notFound('NoSuchKey')
        return { Body: { transformToByteArray: async () => object } }
      },
    },
  }
  return { objects, written, backend: awsBackend(clients) as StoreBackend }
}

/**
 * An editor that saves `saved` and exits with `code`. The child completes on a
 * microtask so the caller has attached its handlers first, as a real spawn does.
 *
 * `meanwhile` runs before it exits, which is where a deploy that starts while
 * someone is sitting in the editor gets to happen.
 */
const editorThatSaves = (saved?: string, { code = 0, meanwhile }: { code?: number; meanwhile?: () => void } = {}) => {
  const opened: { command: string; args: string[] }[] = []
  const spawnProcess = ((command: string, args: string[]) => {
    opened.push({ command, args })
    const handlers: Record<string, (...input: any[]) => void> = {}
    const child = {
      on(event: string, handler: (...input: any[]) => void) {
        handlers[event] = handler
        return child
      },
    }
    queueMicrotask(async () => {
      if (saved !== undefined)
        await (await import('node:fs/promises')).writeFile(args[args.length - 1] as string, saved)
      meanwhile?.()
      handlers.close?.(code, null)
    })
    return child
  }) as any
  return { opened, spawnProcess, file: () => opened[0]?.args.at(-1) as string }
}

const open = ({
  backend,
  editor,
  options = {},
  protect = false,
  environment = { EDITOR: 'fake-editor' },
}: {
  backend: StoreBackend
  editor: ReturnType<typeof editorThatSaves>
  options?: Record<string, string | boolean>
  protect?: boolean
  environment?: NodeJS.ProcessEnv
}) => {
  const lines: string[] = []
  return {
    lines,
    run: () =>
      edit({
        scope: { app: 'boxlite', stage: 'dev', protect } as any,
        options,
        log: (line: string) => lines.push(line),
        backend,
        environment,
        spawnProcess: editor.spawnProcess,
      }),
  }
}

test('the editor is given the stored checkpoint, and what it saves is what is written back', async () => {
  const store = bucket()
  const editor = editorThatSaves(REPAIRED)
  const attempt = open({ backend: store.backend, editor })
  assert.equal(await attempt.run(), 0)

  assert.equal(editor.opened.length, 1)
  assert.equal(editor.opened[0]!.command, 'fake-editor')
  assert.deepEqual(store.written, [{ key: CHECKPOINT_KEY, body: REPAIRED }])
  assert.match(attempt.lines.join('\n'), /^state written for boxlite\/dev$/m)
})

test('how many operations are stuck is said before the editor opens the file', async () => {
  // The reason to open a file this size, printed where it is read: the array to
  // delete from is the one named here.
  const attempt = open({ backend: bucket().backend, editor: editorThatSaves(REPAIRED) })
  assert.equal(await attempt.run(), 0)
  assert.match(attempt.lines[0]!, /^# 1 pending operation in checkpoint\.latest$/)
})

test('an editor that saves nothing writes nothing', async () => {
  const store = bucket()
  const attempt = open({ backend: store.backend, editor: editorThatSaves() })
  assert.equal(await attempt.run(), 0)
  assert.deepEqual(store.written, [], 'an unchanged file is not a change to store')
  assert.match(attempt.lines.join('\n'), /boxlite\/dev is unchanged; nothing was written/)
})

test('a file that is no longer a checkpoint is refused, and the edit is kept and named', async () => {
  const store = bucket()
  const editor = editorThatSaves('{"latest": {}}')
  const attempt = open({ backend: store.backend, editor })
  await assert.rejects(() => attempt.run(), /That is not a checkpoint/)

  assert.deepEqual(store.written, [], 'the stage keeps the state it had')
  const kept = editor.file()
  assert.match(attempt.lines.join('\n'), new RegExp(`the edited state is kept at ${kept}`))
  assert.equal(
    await readFile(kept, 'utf8'),
    '{"latest": {}}',
    'the operator keeps the work, not a copy of the old file',
  )
  await rm(kept, { force: true })
})

test('the wrapper is required in full, not just the word "checkpoint"', async () => {
  // Every one of these parses. Storing any of them would describe a stage with
  // no resources, which is the same as losing the stage.
  const refused = [
    '{"checkpoint":null}',
    '{"checkpoint":{}}',
    '{"checkpoint":"clobbered"}',
    '{"version":3}',
    '{"version":3,"checkpoint":"clobbered"}',
    '{"version":2,"checkpoint":{"latest":{}}}',
    '[{"version":3,"checkpoint":{}}]',
    'not json at all',
  ]
  for (const saved of refused) {
    const store = bucket()
    const editor = editorThatSaves(saved)
    const attempt = open({ backend: store.backend, editor })
    await assert.rejects(() => attempt.run(), /That is not a checkpoint/, saved)
    assert.deepEqual(store.written, [], saved)
    await rm(editor.file(), { force: true })
  }
})

test('a state stored as something other than a checkpoint is reported as one, not counted', async () => {
  // Null rather than zero: a stage whose state no longer parses is stuck for a
  // different reason than one with operations left in flight.
  const attempt = open({ backend: bucket({ checkpoint: '{"checkpoint":{}}' }).backend, editor: editorThatSaves() })
  assert.equal(await attempt.run(), 0)
  assert.match(attempt.lines[0]!, /^# this state does not parse as a checkpoint/)
})

test('a lock taken while the editor is open refuses the write', async () => {
  const store = bucket()
  const editor = editorThatSaves(REPAIRED, {
    meanwhile: () => store.objects.set(LOCK_KEY, Buffer.from(JSON.stringify({ command: 'deploy', runID: '999' }))),
  })
  const attempt = open({ backend: store.backend, editor })
  await assert.rejects(() => attempt.run(), /is locked by deploy in run 999, which happened while this was open/)
  assert.deepEqual(store.written, [], 'the deploy that took the lock keeps its checkpoint')
  assert.match(attempt.lines.join('\n'), /the edited state is kept at/)
  await rm(editor.file(), { force: true })
})

test('a checkpoint rewritten while the editor is open refuses the write', async () => {
  // A lock can be taken and dropped inside one editor session, so the bytes are
  // compared too: an edit that predates a deploy must not land on top of it.
  const store = bucket()
  const editor = editorThatSaves(REPAIRED, {
    meanwhile: () =>
      store.objects.set(CHECKPOINT_KEY, Buffer.from(REPAIRED.replace('Cluster::Cluster', 'Cluster::Two'))),
  })
  const attempt = open({ backend: store.backend, editor })
  await assert.rejects(() => attempt.run(), /boxlite\/dev was rewritten while this was open/)
  assert.deepEqual(store.written, [])
  await rm(editor.file(), { force: true })
})

test('the copy is gone once the write it was made for has landed', async () => {
  const editor = editorThatSaves(REPAIRED)
  assert.equal(await open({ backend: bucket().backend, editor }).run(), 0)
  assert.equal(existsSync(editor.file()), false)
})

test('an edit under a live lock is refused, and points at the command that clears it', async () => {
  const store = bucket({ lock: JSON.stringify({ command: 'deploy', runID: '17512345678' }) })
  const editor = editorThatSaves(REPAIRED)
  await assert.rejects(
    () => open({ backend: store.backend, editor }).run(),
    /is locked by deploy in run 17512345678[\s\S]*npm run mstage state unlock -- --stage dev/,
  )
  assert.equal(editor.opened.length, 0, 'nothing is opened for editing while a deploy may be writing')
  assert.deepEqual(store.written, [])
})

test('a stage nothing has deployed into has no state to edit', async () => {
  await assert.rejects(
    () => open({ backend: bucket({ checkpoint: null }).backend, editor: editorThatSaves(REPAIRED) }).run(),
    /boxlite\/dev has no deployment state/,
  )
})

test('a protected stage is not edited without --confirm', async () => {
  const store = bucket()
  const editor = editorThatSaves(REPAIRED)
  await assert.rejects(() => open({ backend: store.backend, editor, protect: true }).run(), /add --confirm/)
  assert.equal(editor.opened.length, 0)

  const confirmed = open({ backend: store.backend, editor, protect: true, options: { confirm: true } })
  assert.equal(await confirmed.run(), 0)
  assert.deepEqual(store.written, [{ key: CHECKPOINT_KEY, body: REPAIRED }])
})

test('an editor that exits non-zero leaves the stage alone and keeps the file', async () => {
  const store = bucket()
  const editor = editorThatSaves(REPAIRED, { code: 1 })
  const attempt = open({ backend: store.backend, editor })
  await assert.rejects(() => attempt.run(), /fake-editor exited with 1/)
  assert.deepEqual(store.written, [])
  assert.match(attempt.lines.join('\n'), /the edited state is kept at/)
  await rm(editor.file(), { force: true })
})

test('EDITOR is a command line, not a program name', () => {
  assert.deepEqual(editorCommand({ EDITOR: 'code -w' }), ['code', '-w'])
  assert.deepEqual(editorCommand({ EDITOR: '  vi  ' }), ['vi'])
  assert.deepEqual(editorCommand({}), ['vim'], "SST's default, so the two behave alike")
})

test('the state bucket is not named while opening or writing', async () => {
  const attempt = open({ backend: bucket().backend, editor: editorThatSaves(REPAIRED) })
  assert.equal(await attempt.run(), 0)
  assert.ok(!attempt.lines.join('\n').includes(STATE_BUCKET), attempt.lines.join('\n'))
})
