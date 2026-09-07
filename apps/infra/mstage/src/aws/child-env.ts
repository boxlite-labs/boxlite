/**
 * Builds the environment a child process inherits so it sees exactly one AWS identity.
 *
 * mstage resolves the credential itself and hands the child a plain key triple,
 * with every other AWS variable cleared. That matters beyond tidiness on this
 * platform: `aws login` writes `login_session`, which the AWS CLI and the JS SDK
 * understand but the Go SDK behind SST and Pulumi does not — a Go tool started
 * from such a shell falls through to IMDS and finds nothing. Resolving in JS and
 * passing the result down removes the need for a bridge profile entirely.
 *
 * A profile name is never forwarded alongside the triple. The AWS SDK warns
 * about that combination — "Multiple credential sources detected: Both
 * AWS_PROFILE and the pair AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY static
 * credentials are set" — and SST produces it by exporting `AWS_PROFILE` next to
 * its own snapshot (pkg/project/provider/aws.go:54-60), then patches it back out
 * for one command only (cmd/sst/shell.go:125-140).
 *
 * The triple does not refresh, so its expiry is returned for the caller to
 * report or to guard on before starting long work.
 */

import type { AwsIdentity } from './identity.ts'
import type { Scope } from './precedence.ts'

export const childEnvironment = async ({
  scope,
  identity,
  base = process.env,
}: {
  scope: Pick<Scope, 'region' | 'roleArn'>
  identity: Pick<AwsIdentity, 'credentials'>
  base?: NodeJS.ProcessEnv
}): Promise<{ env: NodeJS.ProcessEnv; expiresAt: Date | null }> => {
  const env: NodeJS.ProcessEnv = { ...base, AWS_REGION: scope.region, AWS_DEFAULT_REGION: scope.region }
  delete env.AWS_PROFILE
  delete env.AWS_ACCESS_KEY_ID
  delete env.AWS_SECRET_ACCESS_KEY
  delete env.AWS_SESSION_TOKEN

  const credentials = await identity.credentials()
  env.AWS_ACCESS_KEY_ID = credentials.accessKeyId
  env.AWS_SECRET_ACCESS_KEY = credentials.secretAccessKey
  if (credentials.sessionToken) env.AWS_SESSION_TOKEN = credentials.sessionToken
  return { env, expiresAt: credentials.expiration ?? null }
}

export const runChild = ({
  command,
  args,
  env,
  cwd,
  spawnProcess,
}: {
  command: string
  args: string[]
  env: NodeJS.ProcessEnv
  cwd?: string
  spawnProcess: (command: string, args: string[], options: any) => any
}): Promise<number> =>
  new Promise<number>((resolve, reject) => {
    const child = spawnProcess(command, args, { stdio: 'inherit', env, ...(cwd ? { cwd } : {}) })
    child.on('error', reject)
    child.on('close', (code: number | null, signal: string | null) => {
      if (code === 0) resolve(0)
      else reject(new Error(`${command} exited with ${code ?? signal}`))
    })
  })
