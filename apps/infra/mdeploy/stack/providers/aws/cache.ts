/*
 * Redis on ElastiCache.
 *
 * Shaped like the database's provider and for the same reason: the component
 * mints a password and exposes it as a value, and this copies it into Secrets
 * Manager so the container is handed an address instead. The cache holds
 * sessions and box credentials, so a password sitting in every task definition
 * is worth the one extra resource to avoid.
 *
 * No snapshot, no deletion protection and no retention. A cache that is lost is
 * refilled; the contract says so, and giving it a backup would be paying to
 * keep something nothing reads.
 */

import type { Cache, CacheProvider, CacheRequest } from '../../cache.ts'
import type { NetworkBinding } from '../../network.ts'

const INSTANCE = { small: 'cache.t4g.micro', medium: 'cache.m7g.large' } as const

export const awsCacheProvider =
  ({ network }: { network: Extract<NetworkBinding, { cloud: 'aws' }> }): CacheProvider =>
  (request: CacheRequest): Cache => {
    const redis = new sst.aws.Redis('Cache', {
      vpc: network.vpc,
      cluster: request.clustered,
      instance: INSTANCE[request.size],
      transform: {
        cluster: (args: any) => {
          /*
           * Set from the request rather than left to the component's default.
           * `mdeploy.config.json` refuses `false` outright, so this can only
           * ever be true today — writing it anyway is what makes the setting
           * visible in the plan, and what keeps a future component default
           * change from quietly turning encryption off.
           */
          args.transitEncryptionEnabled = request.encryptInTransit
        },
      },
    })

    const secret = new aws.secretsmanager.Secret('CachePassword', {
      namePrefix: `${$app.name}-${$app.stage}-cache-password-`,
      recoveryWindowInDays: 7,
    })
    const version = new aws.secretsmanager.SecretVersion('CachePasswordValue', {
      secretId: secret.id,
      secretString: $util.secret(redis.password),
    })

    return {
      connection: { host: redis.host, port: redis.port.apply(String) },
      binding: {
        cloud: 'aws',
        passwordRef: version.arn,
        clientGrant: redis.nodes.cluster.securityGroupIds.apply((groups: string[]) => groups[0] as string),
      },
      id: redis.nodes.cluster.id,
      ready: [version],
    }
  }
