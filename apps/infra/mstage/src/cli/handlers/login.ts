/**
 * `mstage login` — can mstage reach AWS with whatever is already signed in.
 *
 * It does not sign anyone in and does not translate the SDK's failure. Sign in
 * with `aws login` (or whatever this machine uses); mstage picks up the result.
 */

import type { AwsIdentity } from '../../aws/identity.ts'

/** Every check takes the same context and ignores what it does not need. */
export type CheckContext = { identity: AwsIdentity }
export type ProviderCheck = (context: CheckContext) => Promise<ProviderStatus>

export type ProviderStatus = {
  provider: string
  state: string
  detail: string
  expiresAt: Date | null
  required?: boolean
}

export const checkAws = async ({ identity }: { identity: AwsIdentity }): Promise<ProviderStatus> => {
  try {
    const caller = await identity.whoami()
    const expiresAt = await identity.expiresAt()
    return {
      provider: 'aws',
      state: 'ready',
      detail: `${caller.principal} in ${caller.tenant}`,
      expiresAt,
    }
  } catch (error) {
    return { provider: 'aws', state: 'not signed in', detail: (error as Error).message, expiresAt: null }
  }
}

/**
 * An optional provider that is not signed in is reported and then forgiven.
 * boxlite-commerce never provisions Auth0 and boxlite-backoffice only does so by
 * hand, so failing the command over it would train people to ignore the output.
 */
export const report = async ({
  statuses,
  log,
}: {
  statuses: ProviderStatus[]
  log: (line: string) => void
}): Promise<number> => {
  let failed = false
  for (const status of statuses) {
    const suffix = status.expiresAt ? ` (expires ${status.expiresAt.toISOString()})` : ''
    const optional = status.required === false ? ' (optional)' : ''
    log(`${status.provider.padEnd(8)}${status.state}${optional}${suffix}`)
    log(`        ${status.detail}`)
    if (status.state !== 'ready' && status.required !== false) failed = true
  }
  return failed ? 1 : 0
}
