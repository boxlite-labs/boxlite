/**
 * The AWS identity a command acts under.
 *
 * Whatever `aws login` (or CI, or a container role) left on the machine is what
 * gets used: this is the SDK's default credential chain and nothing else. mstage
 * adds no selection of its own, does not translate the SDK's errors, and has no
 * opinion about which account the chain reaches. A caller that needs to name the
 * account — an ARN in an IAM document, an ECR host — asks `whoami` for it.
 *
 * The credential *provider* is passed through rather than a resolved key triple,
 * so short-lived sources keep refreshing. SST freezes its resolved triple into
 * `SST_AWS_ACCESS_KEY_ID` at startup (pkg/project/provider/aws.go:48-62) and the
 * platform then uses it with no refresh path
 * (platform/src/components/aws/helpers/client.ts:31).
 */

import { STSClient, GetCallerIdentityCommand } from '@aws-sdk/client-sts'
import { fromNodeProviderChain, fromTemporaryCredentials } from '@aws-sdk/credential-providers'
import type { AwsCredentialIdentityProvider } from '@smithy/types'
import type { Scope } from './precedence.ts'
import type { Caller as TenantCaller, Identity } from '../identity.ts'
import { childEnvironment } from './child-env.ts'

export type Caller = { accountId?: string; arn?: string; userId?: string }

/**
 * The AWS identity: the shared `Identity` plus the one thing only this cloud
 * has. `credentials` is the SDK provider every AWS client is built from.
 */
export type AwsIdentity = Identity & {
  readonly home: 'aws'
  /** Kept for the SDK clients. Nothing outside the AWS backend should read it. */
  credentials: AwsCredentialIdentityProvider
}

export const buildCredentials = (scope: Scope): AwsCredentialIdentityProvider => {
  const base = fromNodeProviderChain({ clientConfig: { region: scope.region } })
  if (!scope.roleArn) return base
  return fromTemporaryCredentials({
    masterCredentials: base,
    params: { RoleArn: scope.roleArn, RoleSessionName: scope.roleSessionName ?? 'mstage' },
    clientConfig: { region: scope.region },
  })
}

export const resolveIdentity = ({
  scope,
  createSts,
  credentialsFor = buildCredentials,
}: {
  scope: Scope
  createSts?: (config: any) => { send: (command: any) => Promise<any>; destroy?: () => void }
  credentialsFor?: (scope: Scope) => AwsCredentialIdentityProvider
}): AwsIdentity => {
  const credentials = credentialsFor(scope)
  const newSts = createSts ?? ((config: any) => new STSClient(config) as any)
  let caller: Caller | null = null

  const whoami = async (): Promise<Caller> => {
    if (caller) return caller
    const client = newSts({ region: scope.region, credentials })
    try {
      const answer = await client.send(new GetCallerIdentityCommand({}))
      caller = { accountId: answer.Account, arn: answer.Arn, userId: answer.UserId }
      return caller
    } finally {
      client.destroy?.()
    }
  }

  const identity: AwsIdentity = {
    home: 'aws',
    credentials,
    region: scope.region,
    stage: scope.stage,
    app: scope.app,
    whoami: async (): Promise<TenantCaller> => {
      const { accountId, arn } = await whoami()
      return { ...(accountId ? { tenant: accountId } : {}), ...(arn ? { principal: arn } : {}) }
    },
    async expiresAt() {
      return (await credentials()).expiration ?? null
    },
    /** Fails before a long operation starts rather than part-way through it. */
    async assertUsableFor(seconds: number, now: () => Date = () => new Date()) {
      const { expiration } = await credentials()
      if (!expiration) return
      const remaining = (expiration.getTime() - now().getTime()) / 1000
      if (remaining < seconds) {
        throw new Error(
          `These credentials expire in ${Math.max(0, Math.round(remaining))}s, ` +
            `less than the ${seconds}s this command needs. Sign in again.`,
        )
      }
    },
    // Three variables and every competing one cleared. They do not refresh,
    // which is why `assertUsableFor` has something to say on this cloud.
    childEnvironment: (base) => childEnvironment({ scope, identity, ...(base ? { base } : {}) }),
  }
  return identity
}
