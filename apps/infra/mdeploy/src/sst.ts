/**
 * How an AWS stage deploys: SST, spawned as a child.
 *
 * What it spends its access on is `mdeploy/sst.config.ts`, which composes
 * mdeploy's own modules — not the `sst.config.ts` beside `mstage.config.json`,
 * which is the incumbent this replaces. SST stays the engine on this cloud
 * because the AWS providers are written against its components —
 * `sst.aws.Vpc`, `sst.aws.Cluster`, `sst.aws.Service` — and nothing else can
 * instantiate those.
 *
 * Both files declare the same app name and stage, and every module keeps the
 * incumbent's logical resource names, so this adopts the existing state instead
 * of building a second copy beside it. That is what makes the cutover a diff to
 * read rather than a migration to perform — and why reading it first is worth
 * the minute: `npm run mdeploy -- --stage <stage> --diff`.
 */

import { spawn } from 'node:child_process'
import { join } from 'node:path'
import { childEnvironment, runChild } from 'mstage/child-env'
import type { AwsIdentity } from 'mstage/aws'
import type { MstageConfig } from 'mstage/config'
import type { StoreBackend } from 'mstage/env'
import type { Scope } from 'mstage/scope'
import { windowFor } from './credential-window.ts'
import { TEARDOWN_GROUPS, rolloutGroups } from './env.ts'
import type { DeployRequest, DeployTarget, Intent } from './deploy.ts'

/** What each intent is called while it runs. */
const NARRATION: Record<Intent, string> = { deploy: 'deploying', diff: 'comparing', remove: 'removing' }

/** Relative to mstage.config.json, which sits at the root of apps/infra. */
const STACK_DIRECTORY = '.'

/** mdeploy's own stack, relative to that directory. */
const STACK_CONFIG = 'mdeploy/sst.config.ts'

export type AwsTargetInput = {
  config: Pick<MstageConfig, 'root' | 'path'>
  scope: Scope
  /**
   * The whole AWS identity, not a cloud-neutral view of one. SST is a Go-SDK
   * tool and inherits a resolved key triple; there is no version of this path
   * that could run on an identity without one.
   */
  identity: Pick<AwsIdentity, 'credentials' | 'assertUsableFor'>
  /** Where this stage's configuration is read from, carried for the caller. */
  backend: StoreBackend
  spawnProcess?: typeof spawn
}

/**
 * An AWS stage, as one bundle.
 *
 * The engine holds the lock, keeps the state and decides the order; mdeploy's
 * part is to hand it one identity, one stage's configuration and one file that
 * describes the stack.
 */
export const awsTarget = ({
  config,
  scope,
  identity,
  backend,
  spawnProcess = spawn,
}: AwsTargetInput): DeployTarget => ({
  cloud: 'aws',
  engine: 'sst',
  backend,
  // SST discovers its state bucket from `/sst/bootstrap` and seals nothing with
  // a passphrase, so there is no engine group here to add.
  environmentGroups: (intent, declaration) => (intent === 'remove' ? TEARDOWN_GROUPS : rolloutGroups(declaration)),

  async run({ intent = 'deploy', targets = [], stageEnvironment = {}, log }: DeployRequest): Promise<number> {
    await identity.assertUsableFor(windowFor(intent))

    const { env: credentials, expiresAt } = await childEnvironment({ scope, identity })
    // The store last, so a stale export in this shell cannot shadow it: the point
    // of keeping configuration in one place is that it wins.
    const env = { ...credentials, ...stageEnvironment }
    const cwd = join(config.root, STACK_DIRECTORY)
    // One flag, comma separated. `--target` is a single string flag that sst
    // splits on commas itself (`cmd/sst/deploy.go`), so repeating the flag keeps
    // only the last value: three components in, one `--target <urn>` out, and the
    // other two silently not deployed.
    const targeting = targets.length > 0 ? ['--target', targets.join(',')] : []
    log(
      `${NARRATION[intent]} ` +
        `${targets.length > 0 ? targets.join(', ') : 'every component'} of ` +
        `${scope.app} stage ${scope.stage} ${intent === 'remove' ? 'from' : 'into'} ${scope.region} ` +
        `from ${STACK_CONFIG}`,
    )
    if (expiresAt) log(`# credentials expire at ${expiresAt.toISOString()}`)

    return runChild({
      command: 'npx',
      args: ['sst', intent, '--stage', scope.stage as string, '--config', STACK_CONFIG, ...targeting],
      env,
      cwd,
      spawnProcess: spawnProcess as any,
    })
  },
})
