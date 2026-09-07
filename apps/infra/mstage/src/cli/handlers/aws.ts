/**
 * `mstage aws` — everything about which AWS identity a stage resolves to.
 *
 * `exec` is the compatibility bridge: it hands the resolved identity to an
 * existing tool rather than replacing it, so anything that reads AWS
 * credentials from the environment keeps working while mstage takes over
 * credential resolution. Which tools a repository runs through it is that
 * repository's decision — mstage obtains access and never names what it is
 * spent on.
 */

import { spawn } from 'node:child_process'
import { childEnvironment, runChild } from '../../aws/child-env.ts'
import type { AwsIdentity } from '../../aws/identity.ts'
import type { Scope } from '../../aws/precedence.ts'

type Log = (line: string) => void

const pad = (key: string) => `${key}:`.padEnd(14)

export const printScope = (scope: Scope, log: Log): void => {
  log(`${pad('stage')}${scope.stage} (${scope.stageSource})`)
  log(`${pad('app')}${scope.app} (${scope.appSource})`)
  log(`${pad('region')}${scope.region} (${scope.regionSource})`)
  if (scope.roleArn) log(`${pad('assume role')}${scope.roleArn} (${scope.roleArnSource})`)
}

export const whoami = async ({ scope, identity, log }: { scope: Scope; identity: AwsIdentity; log: Log }) => {
  const caller = await identity.whoami()
  const expiresAt = await identity.expiresAt()
  printScope(scope, log)
  log(`${pad('account')}${caller.tenant}`)
  log(`${pad('arn')}${caller.principal}`)
  log(`${pad('expires')}${expiresAt ? expiresAt.toISOString() : 'never (long-lived credentials)'}`)
}

export const region = async ({ scope, log }: { scope: Scope; log: Log }) => {
  log(`${scope.region}`)
  log(`# resolved from ${scope.regionSource}`)
}

/** Runs another command under the resolved identity, so existing tools keep working unchanged. */
export const exec = async ({
  scope,
  identity,
  inner,
  log,
  spawnProcess = spawn,
}: {
  scope: Scope
  identity: AwsIdentity
  inner: string[] | null
  log: Log
  spawnProcess?: any
}) => {
  if (!inner || inner.length === 0) {
    throw new Error('usage: npm run mstage aws exec -- --stage <stage> -- <command> [args…]')
  }
  const { env, expiresAt } = await childEnvironment({ scope, identity })
  if (expiresAt) log(`# credentials expire at ${expiresAt.toISOString()}; the child cannot refresh them`)
  const [command, ...args] = inner as string[]
  return runChild({ command: command!, args, env, spawnProcess })
}
