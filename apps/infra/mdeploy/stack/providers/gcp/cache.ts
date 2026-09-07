/*
 * Redis on Memorystore, with a private address.
 *
 * Memorystore mints its own AUTH string and exposes it as an output, so unlike
 * the database this provider copies rather than generates. The destination is
 * the same either way: Secret Manager, and a `secretKeyRef` the platform
 * resolves — the value never becomes a revision's plain environment.
 *
 * `clientGrant` is the account carried by whatever connects, as it is for the
 * database. Memorystore has no IAM role of its own to grant: reachability is
 * the network's answer and the AUTH string is the credential, so what this
 * returns is the identity the firewall rules already name. Saying that plainly
 * beats inventing a grant that does nothing.
 */

import type { Cache, CacheProvider, CacheRequest } from '../../cache.ts'
import type { NetworkBinding } from '../../network.ts'

/** What each requested size answers to, in gigabytes. */
const MEMORY_GB = { small: 1, medium: 5 } as const

const PORT = '6379'

export const gcpCacheProvider =
  ({
    network,
    project,
    region,
    clientAccount,
    dependsOn,
  }: {
    network: Extract<NetworkBinding, { cloud: 'gcp' }>
    project: string
    region: string
    clientAccount: $util.Output<string>
    /** The network's own resources, the peering among them. See `database.ts`. */
    dependsOn: any[]
  }): CacheProvider =>
  (request: CacheRequest): Cache => {
    const prefix = `${$app.name}-${$app.stage}`

    const instance = new gcp.redis.Instance(
      'Cache',
      {
        name: `${prefix}-cache`,
        project,
        region,
        memorySizeGb: MEMORY_GB[request.size],
        // Standard keeps a replica in a second zone; basic is one node. A cache
        // that is lost is refilled, so this follows the request rather than
        // defaulting to the safer answer.
        tier: request.clustered ? 'STANDARD_HA' : 'BASIC',
        authorizedNetwork: network.network,
        connectMode: 'PRIVATE_SERVICE_ACCESS',
        // The AUTH string. Without it the instance admits anything that can
        // reach the address, which on a shared network is more than intended.
        authEnabled: true,
        // `SERVER_AUTHENTICATION` is Memorystore's word for TLS. The contract
        // refuses `false` before this is reached, so this honours the request
        // rather than deciding it.
        transitEncryptionMode: request.encryptInTransit ? 'SERVER_AUTHENTICATION' : 'DISABLED',
      },
      { dependsOn },
    )

    const secret = new gcp.secretmanager.Secret('CachePasswordSecret', {
      project,
      secretId: `${prefix}-cache-password`,
      replication: { auto: {} },
    })
    const version = new gcp.secretmanager.SecretVersion('CachePasswordValue', {
      secret: secret.id,
      secretData: $util.secret(instance.authString),
    })

    return {
      connection: { host: instance.host, port: $util.output(PORT) },
      binding: { cloud: 'gcp', passwordRef: version.name, clientGrant: clientAccount },
      id: instance.id,
      ready: [version],
    }
  }
