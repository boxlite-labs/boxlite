/*
 * The AWS answer to every module, as one bundle.
 *
 * The stack is written against `StackProviders` and names no cloud; this file
 * is where the cloud is chosen, once. It is also the only place the tagged
 * unions are narrowed: `Extract<…, { cloud: 'aws' }>` appears here rather than
 * at a dozen call sites, and each narrowing says which module was handed the
 * wrong cloud rather than failing on a field that does not exist.
 *
 * Nothing here builds a resource. Every entry is a function from the modules it
 * depends on to a provider, so the bundle exists before the first resource does
 * and the stack decides the order.
 */


import type { StackProviders } from '../../index.ts'
import type { Network, NetworkBinding, Placement } from '../../network.ts'
import type { WorkloadHost } from '../../cluster.ts'
import { awsImages } from '../../image.ts'
import { awsAlarmProvider } from './alarms.ts'
import { awsApiProvider } from './api.ts'
import { awsCacheProvider } from './cache.ts'
import { awsClickHouseProvider } from './clickhouse.ts'
import { awsClusterProvider } from './cluster.ts'
import { awsCollectorProvider } from './collector.ts'
import { awsDatabaseProvider } from './database.ts'
import { awsEdgeProvider } from './edge.ts'
import { awsMailProvider } from './mail.ts'
import { awsNetworkProvider } from './network.ts'
import { awsRunnerProvider } from './runners.ts'
import { awsStorageProvider } from './storage.ts'

const onAws = <T extends { cloud: string }>(value: T, what: string): Extract<T, { cloud: 'aws' }> => {
  if (value.cloud !== 'aws') throw new Error(`The AWS stack was handed ${value.cloud} ${what}`)
  return value as Extract<T, { cloud: 'aws' }>
}

const binding = (network: Network): Extract<NetworkBinding, { cloud: 'aws' }> => onAws(network.binding, 'network')
const host = (value: WorkloadHost): Extract<WorkloadHost, { cloud: 'aws' }> => onAws(value, 'host')
const placement = (value: Placement): Extract<Placement, { cloud: 'aws' }> => onAws(value, 'placement')

export const awsStackProviders = ({
  stage,
  region,
  accountId,
  domain,
  artifactsBucket,
  managedClickHouse,
}: {
  stage: string
  region: string
  accountId: string
  /** The root domain the CDN and the API's own hostname sit under. */
  domain: string
  /** Where a build-mode runner binary is staged, for the runner's read grant. */
  artifactsBucket: string
  /**
   * The endpoint and its two secrets for a stage that runs no ClickHouse of its
   * own. Absent rather than empty: a stage with neither has nothing to record,
   * and one with `mode: managed` and no endpoint is refused by the provider.
   */
  managedClickHouse: { url: string; writerSecretArn: string; readerSecretArn: string } | null
}): StackProviders => {
  /*
   * One DNS adapter for every record this stack writes.
   *
   * Cloudflare rather than Route 53, and the same on both clouds — which is the
   * one part of the front door that did not have to be replaced when a second
   * cloud arrived. Built once here because three modules write records and each
   * building its own would be three provider initialisations.
   */
  const dns = sst.cloudflare.dns()

  return {
    // mbuild keys the registry by stage, so the addresses come from there
    // rather than from anything this file composes.
    images: awsImages({ stage, region }),
    network: awsNetworkProvider({ region }),
    storage: awsStorageProvider({ accountId }),
    cluster: ({ network }) => awsClusterProvider({ network }),
    database: ({ network }) => awsDatabaseProvider({ network: binding(network) }),
    cache: ({ network }) => awsCacheProvider({ network: binding(network) }),
    clickhouse: ({ network }) =>
      awsClickHouseProvider({ network: binding(network), region, managed: managedClickHouse }),
    mail: awsMailProvider({ region, dns }),
    collector: ({ host: where, clickhouse, dependsOn }) =>
      awsCollectorProvider({ host: host(where), clickhouse, dependsOn }),
    // The network is ignored here: a security group already says who may reach
    // the API, and repeating it as an identity would be a second answer.
    api: ({ dependencies }) => awsApiProvider({ dependencies, dns, domain }),
    edge: ({ host: where, network, dependsOn }) =>
      awsEdgeProvider({ host: host(where), placement: placement(network.placementFor('proxy')), dns, dependsOn }),
    // No host: a runner is a machine, so it takes its placement straight from
    // the network. That placement is `egress-only-public`, and the provider
    // refuses anything else rather than creating a host that cannot pull.
    runners: ({ network, dependsOn }) =>
      awsRunnerProvider({
        placement: placement(network.placementFor('runner')),
        region,
        artifactsBucket,
        dependsOn,
      }),
    alarms: ({ subjects }) => awsAlarmProvider({ subjects }),
  }
}
