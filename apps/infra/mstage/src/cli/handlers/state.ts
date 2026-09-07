/**
 * `mstage state` — the two repairs a stage needs after a deploy stops halfway.
 *
 * Both are the engine's own — `sst unlock` and `sst state edit` on AWS, and the
 * equivalent repairs to Pulumi's checkpoint and locks on GCP — done against the
 * bucket directly rather than through either CLI. That is deliberate: both have
 * to load a stack config to run them, and which stack this repository deploys is
 * mdeploy's business, not mstage's. The objects, on the other hand, are in the
 * bucket mstage already reads for the stage environment.
 *
 * What mstage adds around them is the rest of a stage: the region it resolves to,
 * the credentials the chain answers with, and the refusal to touch a protected
 * stage without --confirm.
 *
 * What it does not add is a lock of its own. mstage does not deploy, so `edit`
 * is for a stage nothing is deploying into: it refuses to open while a lock is
 * held, and refuses to write if a lock was taken or the checkpoint moved while
 * the editor was open. That is narrower than holding the lock — the write is not
 * atomic — but it covers the window that is minutes long rather than the one
 * that is milliseconds long.
 */

import { spawn } from 'node:child_process'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { runChild } from '../../aws/child-env.ts'
import {
  StateError,
  clearLock,
  describeLock,
  pendingOperations,
  readCheckpoint,
  readLock,
  writeCheckpoint,
} from '../../state/store.ts'
import type { StoreBackend } from '../../env/backend.ts'
import type { Scope } from '../../aws/precedence.ts'

type Log = (line: string) => void

type Input = {
  scope: Scope
  options: Record<string, string | boolean>
  log: Log
  /** The store this stage lives in. Resolved once, by `resolveHome`. */
  backend: StoreBackend
}

const refuseProtected = (scope: Scope, options: Input['options'], what: string): void => {
  if (scope.protect && options.confirm !== true) {
    throw new StateError(`Stage "${scope.stage}" is protected; add --confirm to ${what}`)
  }
}

/**
 * `mstage state unlock` — drops the lock a deploy did not live to release.
 *
 * What held it is printed before it goes, because the one thing this command
 * cannot tell is whether that deploy is still running somewhere. A lock removed
 * out from under a live deploy lets a second one start against the same
 * checkpoint, so the operator gets whatever the engine recorded — the command
 * and run id from SST, the user, host and pid from Pulumi — and makes that call.
 */
export const unlock = async ({ scope, options, log, backend }: Input): Promise<number> => {
  refuseProtected(scope, options, 'drop its lock')

  const app = scope.app as string
  const stage = scope.stage as string
  const lock = await readLock({ backend, app, stage })
  if (!lock) {
    // Not an error: the caller asked for the lock to be gone, and it is.
    log(`# ${app}/${stage} holds no lock`)
    return 0
  }
  log(describeLock({ app, stage, lock }))
  const removed = await clearLock({ backend, app, stage, replacing: lock })
  log(removed ? `lock removed from ${app}/${stage}` : `# that lock went before it could be removed`)
  return 0
}

/**
 * What opens the file.
 *
 * `EDITOR` is a command line rather than a program name — `code -w` and `subl -w`
 * are how a windowed editor is made to wait — so it is split the way a shell
 * would split it. The fallback is vim, which is SST's (cmd/sst/state.go).
 */
export const editorCommand = (environment: NodeJS.ProcessEnv): [string, ...string[]] => {
  const [program, ...arguments_] = (environment.EDITOR ?? '').trim().split(/\s+/).filter(Boolean)
  return program ? [program, ...arguments_] : ['vim']
}

/**
 * `mstage state edit` — the checkpoint itself, in an editor.
 *
 * The escape hatch for a state no deploy will accept, and pending operations are
 * the usual reason: deleting them from `checkpoint.latest.pending_operations` is
 * what lets the next deploy plan again. How many there are is printed first, so
 * the reason for opening a file this size is visible before the editor is.
 *
 * The copy is written to a private temporary directory and removed once the
 * write it was made for has landed. A write that is refused keeps it, and says
 * where: the edit is the operator's work and that copy is the only place it
 * exists.
 */
export const edit = async ({
  scope,
  options,
  log,
  backend,
  environment = process.env,
  spawnProcess = spawn,
}: Input & { environment?: NodeJS.ProcessEnv; spawnProcess?: typeof spawn }): Promise<number> => {
  refuseProtected(scope, options, 'edit its state')

  const app = scope.app as string
  const stage = scope.stage as string
  // A lock means something may be deploying into this checkpoint right now, and
  // an edit under one is how two writers end up with one file between them.
  const lock = await readLock({ backend, app, stage })
  if (lock) {
    throw new StateError(
      `${describeLock({ app, stage, lock })}. An edit would be overwritten by that deploy, or overwrite it. ` +
        `If it is gone, drop the lock first: npm run mstage state unlock -- --stage ${stage}`,
    )
  }

  const stored = await readCheckpoint({ backend, app, stage })
  const pending = pendingOperations(stored)
  if (pending === null) log(`# this state does not parse as a checkpoint, which is itself enough to stop a deploy`)
  else if (pending > 0) log(`# ${pending} pending operation${pending === 1 ? '' : 's'} in checkpoint.latest`)

  const workspace = await mkdtemp(join(tmpdir(), 'mstage-state-'))
  const file = join(workspace, `${stage}.json`)
  let settled = false
  try {
    await writeFile(file, stored, { mode: 0o600 })
    const [command, ...args] = editorCommand(environment)
    await runChild({ command, args: [...args, file], env: environment, spawnProcess })

    const edited = await readFile(file)
    if (edited.equals(stored)) {
      log(`# ${app}/${stage} is unchanged; nothing was written`)
    } else {
      await writeCheckpoint({ backend, app, stage, checkpoint: edited, replacing: stored })
      log(`state written for ${app}/${stage}`)
      // Said every time, because the edit only makes the stage deployable. What
      // the interrupted operations left in the cloud is still unobserved, and a
      // refresh before the next deploy is what reconciles it.
      log('# the record is deployable again; a refresh is what makes it true')
    }
    settled = true
  } finally {
    if (settled) await rm(workspace, { recursive: true, force: true })
    else log(`# the edited state is kept at ${file}`)
  }
  return 0
}
