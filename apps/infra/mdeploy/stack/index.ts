/*
 * The whole stack, assembled once.
 *
 * Every module above this file describes one thing and knows nothing about the
 * others; this is the only place that knows the order they go in and what each
 * one is handed. That order is not a style choice — it is the dependency graph,
 * and writing it down once is what keeps eleven modules from each having an
 * opinion about it:
 *
 *   images ──→ api, proxy, collector                (addresses for one commit)
 *   network ──→ cluster ──→ hosts
 *           ├──→ database ─┐
 *           ├──→ cache ────┼──→ api
 *           └──→ clickhouse ┴──→ collector ──→ api, proxy, runners
 *   storage ────────────────────────────────→ api
 *   mail ───────────────────────────────────→ api
 *   api ──→ proxy, runners ──→ alarms
 *
 * The providers arrive as one bundle rather than being imported here, so this
 * file names no cloud. That is what makes it testable without one: a bundle of
 * recording fakes drives the same wiring the real bundle does.
 */

import type { Api, ApiCapability, ApiDependencies, ApiProvider } from './api.ts'
import { API_PORT } from './api.ts'
import type { AlarmProvider, AlarmSubjects } from './alarms.ts'
import type { Cache, CacheProvider } from './cache.ts'
import { CACHE_PASSWORD_VARIABLE, cacheEnvironment } from './cache.ts'
import type { ClickHouse, ClickHouseProvider } from './clickhouse.ts'
import { CLICKHOUSE_PASSWORD_VARIABLE, clickHouseEnvironment } from './clickhouse.ts'
import type { Cluster, ClusterProvider, WorkloadHost } from './cluster.ts'
import type { Collector, CollectorProvider } from './collector.ts'
import type { Database, DatabaseProvider } from './database.ts'
import { DATABASE_PASSWORD_VARIABLE, databaseEnvironment } from './database.ts'
import type { Edge, EdgeProvider } from './edge.ts'
import type { Images, ImagesRequest } from './image.ts'
import type { Mail, MailProvider } from './mail.ts'
import { mailEnvironment } from './mail.ts'
import type { Network, NetworkProvider } from './network.ts'
import type { RunnerProvider, RunnerSlot, Runners } from './runners.ts'
import type { Storage, StorageProvider } from './storage.ts'
import type { DeployConfig } from '../src/config.ts'
import { alarmsFor, cacheFor, clickHouseFor, databaseFor, runnersFor, storageFor } from '../src/config.ts'

/** The containerised workloads. The runner is a machine and is placed directly. */
const CONTAINER_ROLES = ['api', 'proxy', 'otel-collector'] as const

/**
 * The image each workload pulls, keyed by the artifact name
 * `mbuild.config.json` declares. A missing component is a build that did not
 * publish rather than an address this file could invent.
 */
const imageFor = (images: Images, component: 'api' | 'proxy' | 'otel-collector'): $util.Input<string> => {
  const address = images[component]
  if (!address) throw new Error(`mbuild published no ${component} image for this commit`)
  return address
}

/** What one deploy decides, as opposed to what `mdeploy.config.json` declares. */
export type StackInputs = {
  stage: string
  /** The commit being deployed. Every container runs the same one. */
  tag: string
  /** The hostname the dashboard and the SDKs reach the control plane on. */
  domain: string
  /** The zone `*.<proxyDomain>` is written into. Every box is a name under it. */
  proxyDomain: string
  /** What the proxy speaks to a box. */
  proxyProtocol: string
  /** Whether private workloads get outbound internet. */
  internetEgress: boolean
  /** The verified sender domain, or null for a stage that sends no mail. */
  senderDomain: string | null
  /** What each runner installs, resolved before any host exists. */
  runnerBinary: import('./runners.ts').RunnerBinary
  /** The hosts to create, and the names the control plane registers them under. */
  runnerFleet: readonly RunnerSlot[]
  /** Each workload's own environment and addresses, split by the store's groups. */
  apiEnvironment: Record<string, $util.Input<string>>
  apiSecrets: Record<string, $util.Input<string>>
  proxyEnvironment: Record<string, $util.Input<string>>
  proxySecrets: Record<string, $util.Input<string>>
  collectorEnvironment: Record<string, $util.Input<string>>
  collectorSecrets: Record<string, $util.Input<string>>
  runnerEnvironment: Record<string, $util.Input<string>>
  runnerSecrets: Record<string, $util.Input<string>>
}

/**
 * The providers, already parameterised with whatever their cloud needs. Each
 * entry is a function from the modules it depends on to the provider itself,
 * which is why the bundle can be built before any resource exists.
 */
export type StackProviders = {
  images: (request: ImagesRequest) => Images
  network: NetworkProvider
  storage: StorageProvider
  cluster: (input: { network: Network }) => ClusterProvider
  database: (input: { network: Network }) => DatabaseProvider
  cache: (input: { network: Network }) => CacheProvider
  clickhouse: (input: { network: Network }) => ClickHouseProvider
  mail: MailProvider
  collector: (input: {
    host: WorkloadHost
    network: Network
    clickhouse: ClickHouse
    dependsOn: any[]
  }) => CollectorProvider
  /**
   * Handed the network as well as its dependencies, because the API is private
   * either way and the two clouds say so differently. AWS puts it behind a
   * security group the network already arranged; Cloud Run refuses everyone
   * except a named invoker, and that name comes from the network too.
   */
  api: (input: { dependencies: ApiDependencies; network: Network }) => ApiProvider
  edge: (input: { host: WorkloadHost; network: Network; dependsOn: any[] }) => EdgeProvider
  /**
   * No host: a runner is a machine rather than a task, so it takes a placement
   * from the network directly. See `cluster.ts`.
   */
  runners: (input: { network: Network; dependsOn: any[] }) => RunnerProvider
  alarms: (input: { subjects: AlarmSubjects }) => AlarmProvider
}

/**
 * A name one workload reads is carried by one channel and no other.
 *
 * Every channel becomes an environment variable in the same container, so a
 * name in two of them is two entries for one variable — and the clouds do not
 * agree about that: an ECS task takes the last one written, a Cloud Run
 * revision will not accept the pair at all. Refused here, where every channel
 * is known.
 *
 * `injected` is why this takes four arguments rather than comparing the two
 * maps. The database password, the cache password and the ClickHouse password
 * are handed to their containers by reference under names that never appear in
 * `environment`, so comparing only those two would let a store address for one
 * through — to be dropped on one cloud and rejected on the other, which is the
 * divergence this exists to prevent.
 */
const assertOneChannelPerName = ({
  workload,
  environment,
  secrets,
  injected,
}: {
  /** The container this is about, as the refusal should name it. */
  workload: string
  environment: Record<string, $util.Input<string>>
  secrets: Record<string, $util.Input<string>>
  /** Names the stack hands the workload by reference itself. */
  injected: readonly string[]
}): void => {
  const carried = new Set([...Object.keys(environment), ...injected])
  const both = Object.keys(secrets).filter((name) => carried.has(name))
  if (both.length > 0) {
    throw new Error(
      `${both.join(', ')} would reach the ${workload} twice — the stack carries that name itself, ` +
        'and the store names it as an address; a name is carried by one channel or the other',
    )
  }
}

/** What the deploy reports, and what a post-deploy check reads. */
export type StackOutputs = {
  apiUrl: $util.Output<string>
  proxyUrl: $util.Output<string>
  databaseId: $util.Output<string>
  cacheId: $util.Output<string>
  storageName: $util.Output<string>
  collectorUrl: $util.Output<string>
  clickHouseId: $util.Output<string> | null
  runnerIds: $util.Output<string>[]
}

export const deployStack = ({
  providers,
  config,
  inputs,
}: {
  providers: StackProviders
  config: DeployConfig
  inputs: StackInputs
}): StackOutputs => {
  const images = providers.images({ tag: inputs.tag })

  const network = providers.network({ internetEgress: inputs.internetEgress })
  const storage: Storage = providers.storage(storageFor(config, inputs.stage))
  const cluster: Cluster = providers.cluster({ network })({ roles: CONTAINER_ROLES })
  const database: Database = providers.database({ network })(databaseFor(config, inputs.stage))
  const cache: Cache = providers.cache({ network })(cacheFor(config, inputs.stage))
  const clickhouse: ClickHouse = providers.clickhouse({ network })(clickHouseFor(config, inputs.stage))

  /*
   * The network's own rules, which reach every workload through the cluster.
   * Outbound HTTPS is how any task pulls an image or reaches the OIDC issuer,
   * and the proxy↔API pair is how a box lookup succeeds; a task placed before
   * they exist starts, fails, and is replaced in a loop. Nothing else consumes
   * `cluster.ready`, so every workload waits on it here.
   */
  const placed = cluster.ready

  // Ahead of the API because it produces that service's SMTP settings, and
  // because verification blocks on DNS rather than on anything this stack
  // builds — so a first deploy of a new sender domain waits here rather than at
  // the first invitation.
  const mail: Mail = providers.mail({ senderDomain: inputs.senderDomain })

  // Before the API, so every workload emits to the same collector — including
  // the API's own traces, which is why it is not the other way round.
  const collectorEnvironment = {
    ...inputs.collectorEnvironment,
    ...clickHouseEnvironment(clickhouse, 'writer'),
  }
  assertOneChannelPerName({
    workload: 'collector',
    environment: collectorEnvironment,
    secrets: inputs.collectorSecrets,
    injected: [CLICKHOUSE_PASSWORD_VARIABLE],
  })
  const collector: Collector = providers.collector({
    host: cluster.hostFor('otel-collector'),
    network,
    clickhouse,
    dependsOn: [...placed, ...(clickhouse.active ? clickhouse.ready : [])],
  })({
    image: imageFor(images, 'otel-collector'),
    // One list for all three pipelines. A collector told to write to a
    // ClickHouse that does not exist retries every batch forever, so a disabled
    // stage runs the BoxLite exporter alone.
    exporters: clickhouse.active ? '[boxlite_exporter,clickhouse]' : '[boxlite_exporter]',
    environment: collectorEnvironment,
    secrets: inputs.collectorSecrets,
  })

  // The stack's own names last: a stale copy of a database host in the store
  // must not shadow the database this deploy just built.
  const apiEnvironment = {
    ...inputs.apiEnvironment,
    ...databaseEnvironment(database),
    ...cacheEnvironment(cache),
    ...mailEnvironment(mail),
    ...clickHouseEnvironment(clickhouse, 'reader'),
    DB_TLS_ENABLED: 'true',
    OTEL_EXPORTER_OTLP_ENDPOINT: collector.otlpUrl,
    BOX_OTEL_ENDPOINT_URL: collector.otlpUrl,
    PROXY_DOMAIN: inputs.proxyDomain,
    PROXY_PROTOCOL: inputs.proxyProtocol,
    S3_DEFAULT_BUCKET: storage.name,
  }
  assertOneChannelPerName({
    workload: 'API',
    environment: apiEnvironment,
    secrets: inputs.apiSecrets,
    /*
     * The three the stack mints and hands over by reference.
     *
     * The SMTP credential is deliberately not among them, and the asymmetry is
     * worth stating: an SES SMTP credential is an IAM user's access key, which
     * the deploy role cannot create — so it is seeded out of band and reaches
     * the API through the store's own `api` group like any other secret. What
     * the mail module contributes is the host and the From address, which is
     * why `mailEnvironment` carries those and nothing else.
     */
    injected: [DATABASE_PASSWORD_VARIABLE, CACHE_PASSWORD_VARIABLE, CLICKHOUSE_PASSWORD_VARIABLE],
  })

  const api: Api = providers.api({
    network,
    dependencies: {
      host: cluster.hostFor('api'),
      placement: network.placementFor('api'),
      database,
      cache,
      storage,
      clickhouse,
      waitFor: [...placed, ...database.ready, ...cache.ready, ...collector.ready, ...(mail.sending ? mail.ready : [])],
    },
  })({
    image: imageFor(images, 'api'),
    port: API_PORT,
    domain: inputs.domain,
    environment: apiEnvironment,
    secrets: inputs.apiSecrets,
    capabilities: capabilitiesFor({ storage, clickhouse }),
  })

  const proxyEnvironment = {
    ...inputs.proxyEnvironment,
    OTEL_EXPORTER_OTLP_ENDPOINT: collector.otlpUrl,
  }
  assertOneChannelPerName({
    workload: 'proxy',
    environment: proxyEnvironment,
    secrets: inputs.proxySecrets,
    injected: [],
  })
  const edge: Edge = providers.edge({
    host: cluster.hostFor('proxy'),
    network,
    // The proxy is a front door, so it comes up after what it proxies to: it
    // must not accept a box connection before the API can say which runner
    // holds that box.
    dependsOn: [...placed, ...api.ready],
  })({
    image: imageFor(images, 'proxy'),
    domain: inputs.proxyDomain,
    protocol: inputs.proxyProtocol,
    apiUrl: api.address,
    environment: proxyEnvironment,
    secrets: inputs.proxySecrets,
  })

  const runnerEnvironment = {
    ...inputs.runnerEnvironment,
    OTEL_EXPORTER_OTLP_ENDPOINT: collector.otlpUrl,
  }
  assertOneChannelPerName({
    workload: 'runner',
    environment: runnerEnvironment,
    secrets: inputs.runnerSecrets,
    injected: [],
  })
  const runners: Runners = providers.runners({
    network,
    // A host registers itself with the control plane at first boot, so the
    // control plane has to be answering before one exists.
    dependsOn: [...api.ready],
  })({
    ...runnersFor(config, inputs.stage),
    nestedVirtualization: true,
    fleet: inputs.runnerFleet,
    binary: inputs.runnerBinary,
    apiUrl: api.address,
    otlpUrl: collector.otlpUrl,
    environment: runnerEnvironment,
    secrets: inputs.runnerSecrets,
  })

  // Alarms watch what is already serving, so nothing waits on them: one that
  // cannot be created must not roll back a service that is answering.
  providers.alarms({ subjects: { api, edge, runners } })(alarmsFor(config, inputs.stage))

  return {
    apiUrl: api.url,
    proxyUrl: edge.url,
    databaseId: database.id,
    cacheId: cache.id,
    storageName: storage.name,
    collectorUrl: collector.otlpUrl,
    clickHouseId: clickhouse.active ? clickhouse.id : null,
    runnerIds: runners.ids,
  }
}

/**
 * What the API may do.
 *
 * List its own bucket, which is the boot probe; create, tag and delete the
 * volume buckets it owns, which is bounded by the storage module's prefix; vend
 * a scoped credential for one of them; and read telemetry back, which exists
 * only where a stage keeps any. Nothing here names an ARN or a role — the
 * provider bundle expands each sentence into its cloud's own grants.
 */
export const capabilitiesFor = ({
  storage,
  clickhouse,
}: {
  storage: Storage
  clickhouse: ClickHouse
}): ApiCapability[] => [
  { kind: 'list-own-bucket', storage },
  { kind: 'manage-volume-buckets', storage },
  { kind: 'vend-volume-credentials', storage },
  ...(clickhouse.active ? ([{ kind: 'read-telemetry', clickhouse }] as ApiCapability[]) : []),
]
