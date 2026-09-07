/*
 * The GCP answer to every module, as one bundle.
 *
 * The mirror of `../aws/index.ts`, and deliberately the same shape: the stack
 * is written against `StackProviders` and names no cloud, so choosing GCP is
 * choosing this file instead of that one. It is also the only place the tagged
 * unions are narrowed to `{ cloud: 'gcp' }`, so a module handed an AWS network
 * says so by name rather than failing on a field that is not there.
 *
 * Two asymmetries are real and are not hidden, and both come from the same
 * fact: Google authorises by identity where AWS authorises by position. Every
 * Cloud Run service here has to be told which accounts may invoke it, because
 * there is no security group to say it — so the API is handed the proxy's and
 * the runner's identities, and the collector is handed all three. On AWS
 * nothing is passed, because the network already arranged it.
 *
 * Nothing here has been deployed. Every provider typechecks against
 * `@pulumi/gcp`; there is no project, no network and no billing to run them
 * against.
 */

import type { StackProviders } from '../../index.ts'
import type { Network, NetworkBinding, Placement } from '../../network.ts'
import { gcpImages } from '../../image.ts'
import { gcpAlarmProvider } from './alarms.ts'
import { gcpApiProvider } from './api.ts'
import { gcpCacheProvider } from './cache.ts'
import { gcpClickHouseProvider } from './clickhouse.ts'
import { gcpClusterProvider } from './cluster.ts'
import { gcpCollectorProvider } from './collector.ts'
import { gcpDatabaseProvider } from './database.ts'
import { gcpEdgeProvider } from './edge.ts'
import { gcpMailProvider } from './mail.ts'
import { gcpNetworkProvider } from './network.ts'
import { gcpRunnerProvider } from './runners.ts'
import { gcpStorageProvider } from './storage.ts'

const onGcp = <T extends { cloud: string }>(value: T, what: string): Extract<T, { cloud: 'gcp' }> => {
  if (value.cloud !== 'gcp') throw new Error(`The GCP stack was handed ${value.cloud} ${what}`)
  return value as Extract<T, { cloud: 'gcp' }>
}

const binding = (network: Network): Extract<NetworkBinding, { cloud: 'gcp' }> => onGcp(network.binding, 'network')
const placement = (network: Network, role: Parameters<Network['placementFor']>[0]): Extract<Placement, { cloud: 'gcp' }> =>
  onGcp(network.placementFor(role), 'placement')

/**
 * A zone in the stage's region.
 *
 * An instance is zonal even where a subnet is not, so the two resources that
 * are machines — ClickHouse and the runners — need one. Derived rather than
 * declared: `mstage.config.json` names a region, and a second setting for the
 * zone inside it would be one more thing to keep in step for a value that only
 * ever means "the first one".
 */
const zoneIn = (region: string): string => `${region}-a`

export const gcpStackProviders = ({
  stage,
  region,
  project,
  domain,
  zoneId,
  relayHost = null,
  managedClickHouse = null,
  notificationChannels = [],
}: {
  stage: string
  region: string
  project: string
  /** The hostname the dashboard and the SDKs reach the control plane on. */
  domain: string
  /** The Cloudflare zone every public record is written into. */
  zoneId: string
  /** The SMTP relay a GCP stage sends through. Google provides none. */
  relayHost?: string | null
  managedClickHouse?: { url: string; writerSecretArn: string; readerSecretArn: string } | null
  notificationChannels?: string[]
}): StackProviders => {
  const zone = zoneIn(region)

  return {
    images: gcpImages({ stage, region, project }),
    network: gcpNetworkProvider({ project, region }),
    storage: gcpStorageProvider({ project, region }),
    // Builds nothing: Cloud Run has no cluster. It still carries the network's
    // rules, which every workload waits on.
    cluster: ({ network }) => gcpClusterProvider({ region, network }),
    database: ({ network }) =>
      gcpDatabaseProvider({
        network: binding(network),
        project,
        region,
        clientAccount: placement(network, 'api').serviceAccount,
        dependsOn: network.ready,
      }),
    cache: ({ network }) =>
      gcpCacheProvider({
        network: binding(network),
        project,
        region,
        clientAccount: placement(network, 'api').serviceAccount,
        dependsOn: network.ready,
      }),
    clickhouse: ({ network }) =>
      gcpClickHouseProvider({
        network: binding(network),
        project,
        zone,
        // The collector writes and the API reads; both carry an account, and
        // the firewall admits those two and nothing else.
        serviceAccount: placement(network, 'otel-collector').serviceAccount,
        managed: managedClickHouse,
        dependsOn: network.ready,
      }),
    mail: gcpMailProvider({ relayHost }),
    collector: ({ network, clickhouse, dependsOn }) =>
      gcpCollectorProvider({
        project,
        region,
        placement: placement(network, 'otel-collector'),
        clickhouse,
        // Everything that ships telemetry, named. There is no security group
        // to say it for us.
        callers: [
          placement(network, 'api').serviceAccount,
          placement(network, 'proxy').serviceAccount,
          placement(network, 'runner').serviceAccount,
        ],
        dependsOn,
      }),
    api: ({ dependencies, network }) =>
      gcpApiProvider({
        dependencies,
        project,
        region,
        domain,
        // The proxy resolves a box through it; a runner registers itself with
        // it. Cloud Run admits named invokers and nobody else.
        callers: [placement(network, 'proxy').serviceAccount, placement(network, 'runner').serviceAccount],
        zoneId,
      }),
    // Not a Cloud Run service: the proxy reads the SNI name itself, which needs
    // a layer-4 path no Cloud Run front end offers. See `edge.ts`.
    edge: ({ network, dependsOn }) =>
      gcpEdgeProvider({ project, region, placement: placement(network, 'proxy'), zoneId, dependsOn }),
    runners: ({ network, dependsOn }) =>
      gcpRunnerProvider({ project, zone, placement: placement(network, 'runner'), dependsOn }),
    alarms: ({ subjects }) => gcpAlarmProvider({ subjects, project, notificationChannels }),
  }
}
