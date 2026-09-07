/*
 * The composition root, driven by recording fakes.
 *
 * `stack/index.ts` names no cloud, which is exactly what makes this possible: a
 * bundle of fakes satisfies `StackProviders` as well as either real bundle
 * does, so the order the modules go in and what each one is handed can be
 * checked without an account, a network, or either engine.
 *
 * What is checked here is the wiring — that a module is handed the handles it
 * depends on, that the stack's own names win over the store's, and that a name
 * carried by two channels is refused. What is not checked is any resource's
 * arguments; those live in the provider bundles, which need their cloud.
 */

import assert from 'node:assert/strict'
import test from 'node:test'
import { deployStack, type StackInputs, type StackProviders } from '../stack/index.ts'
import { parseDeployConfig, type DeployConfig } from '../src/config.ts'
import type { ClickHouse } from '../stack/clickhouse.ts'

/** A resolved value, in the shape the contracts describe. */
const out = <T>(value: T): any => ({ apply: (fn: (v: T) => unknown) => out(fn(value)), __output: value })

/** What a fake handed back, unwrapped. */
const read = (value: any): unknown => (value && typeof value === 'object' && '__output' in value ? value.__output : value)

const config: DeployConfig = parseDeployConfig(
  '/repo/apps/infra/mdeploy.config.json',
  JSON.stringify({
    database: { name: 'boxlite', size: 'small', highlyAvailable: false, backupRetentionDays: 7, protected: false },
    cache: { size: 'small', clustered: false, encryptInTransit: true },
    storage: { volumePrefix: 'boxlite-volume', versioning: true },
    clickhouse: {
      mode: 'self-hosted',
      database: 'otel',
      writerUsername: 'otel_writer',
      readerUsername: 'otel_reader',
      instanceSize: 'medium',
      dataGb: 50,
    },
    runners: { size: 'large', rootDiskGb: 100 },
    alarms: {
      apiServerErrors: { threshold: 1, periods: 1 },
      proxyUnhealthyTargets: { threshold: 1, periods: 2 },
      runnersUnreachable: { threshold: 1, periods: 3 },
    },
  }),
)

const inputs = (overrides: Partial<StackInputs> = {}): StackInputs => ({
  stage: 'dev',
  tag: 'a'.repeat(40),
  domain: 'dev.boxlite.ai',
  proxyDomain: 'box.dev.boxlite.ai',
  proxyProtocol: 'http',
  internetEgress: true,
  senderDomain: 'mail.dev.boxlite.ai',
  runnerBinary: { url: 'https://example.invalid/runner.tar.gz', sha256: 'b'.repeat(64), source: 'release' },
  runnerFleet: [{ resourceName: 'Runner', nameTag: 'boxlite-runner-default', controlPlaneRunnerName: 'default' }],
  apiEnvironment: {},
  apiSecrets: {},
  proxyEnvironment: {},
  proxySecrets: {},
  collectorEnvironment: {},
  collectorSecrets: {},
  runnerEnvironment: {},
  runnerSecrets: {},
  ...overrides,
})

const clickHouseFake = (active: boolean): ClickHouse =>
  active
    ? {
        active: true,
        mode: 'self-hosted',
        url: out('http://10.0.0.9:8123'),
        database: 'otel',
        writer: { username: 'otel_writer', passwordRef: out('writer-ref'), credentialVersion: out('v1') },
        reader: { username: 'otel_reader', passwordRef: out('reader-ref'), credentialVersion: out('v1') },
        binding: { cloud: 'aws', clientGrant: out('sg-clickhouse') },
        id: out('i-clickhouse'),
        ready: ['clickhouse'],
      }
    : { active: false, mode: 'disabled' }

/**
 * A bundle that records what each module was handed.
 *
 * Every entry answers with a plausible handle rather than a stub that throws,
 * because the stack reads these — `api.address` reaches the proxy and the
 * runner, `collector.otlpUrl` reaches three workloads — and a fake that
 * returned nothing would pass a wiring check it never exercised.
 */
const bundle = ({ clickhouse = true }: { clickhouse?: boolean } = {}) => {
  const seen: Record<string, any> = {}
  const order: string[] = []
  const record = (name: string, value: any) => {
    order.push(name)
    seen[name] = value
    return value
  }
  const network = {
    binding: { cloud: 'aws' as const, cidr: out('10.0.0.0/16') },
    placementFor: (role: string) => ({ cloud: 'aws' as const, exposure: role === 'runner' ? 'egress-only-public' : 'private', role }),
    ready: ['network-rules'],
  }
  const storage = {
    name: out('boxlite-dev-storage'),
    binding: { cloud: 'aws' as const, credentialVending: { roleName: 'r', roleArn: out('arn:role') } },
    ready: [],
  }
  const database = {
    connection: { host: out('db.internal'), port: out('5432'), username: out('boxlite'), database: out('boxlite') },
    binding: { cloud: 'aws' as const, passwordRef: out('db-ref'), clientGrant: out('sg-db') },
    id: out('db-1'),
    ready: ['db'],
  }
  const cache = {
    connection: { host: out('cache.internal'), port: out('6379') },
    binding: { cloud: 'aws' as const, passwordRef: out('cache-ref'), clientGrant: out('sg-cache') },
    id: out('cache-1'),
    ready: ['cache'],
  }
  const providers = {
    images: (request: any) =>
      record('images', {
        api: out(`registry/${request.tag}-api`),
        proxy: out(`registry/${request.tag}-proxy`),
        'otel-collector': out(`registry/${request.tag}-otel`),
      }),
    network: (request: any) => record('network', { ...network, request }),
    storage: (request: any) => record('storage', { ...storage, request }),
    cluster: (input: any) => (request: any) =>
      record('cluster', {
        input,
        request,
        hostFor: (role: string) => ({ cloud: 'aws' as const, role }),
        ready: input.network.ready,
      }),
    database: (input: any) => (request: any) => record('database', { ...database, input, request }),
    cache: (input: any) => (request: any) => record('cache', { ...cache, input, request }),
    clickhouse: (input: any) => (request: any) =>
      record('clickhouse', { ...clickHouseFake(clickhouse), input, request }),
    mail: (request: any) =>
      record('mail', {
        sending: Boolean(request.senderDomain),
        verified: true,
        host: 'smtp.invalid',
        port: '465',
        senderAddress: `no-reply@${request.senderDomain}`,
        ready: ['mail'],
        request,
      }),
    collector: (input: any) => (request: any) =>
      record('collector', { input, request, otlpUrl: out('http://collector:4318'), ready: ['collector'] }),
    api: (input: any) => (request: any) =>
      record('api', {
        input,
        request,
        url: out('https://dev.boxlite.ai'),
        address: out('https://api.dev.boxlite.ai'),
        identity: out('arn:task-role'),
        metricTarget: out('app/api/1'),
        ready: ['api'],
      }),
    edge: (input: any) => (request: any) =>
      record('edge', { input, request, url: out('https://box.dev.boxlite.ai'), metricTarget: out('net/proxy/1'), ready: ['edge'] }),
    runners: (input: any) => (request: any) =>
      record('runners', { input, request, ids: [out('i-runner')], ready: ['runners'] }),
    alarms: (input: any) => (request: any) => record('alarms', { input, request }),
  } as unknown as StackProviders
  return { providers, seen, order }
}

test('every module is handed the modules it depends on, and nothing else builds first', () => {
  const { providers, order } = bundle()
  deployStack({ providers, config, inputs: inputs() })

  // The graph, read back out of what each provider was handed.
  assert.ok(order.indexOf('network') < order.indexOf('cluster'), 'the cluster is placed in the network')
  assert.ok(order.indexOf('database') < order.indexOf('api'), 'the API queries tables')
  assert.ok(order.indexOf('collector') < order.indexOf('api'), 'the API emits to the collector')
  assert.ok(order.indexOf('api') < order.indexOf('edge'), 'the proxy resolves a box through the API')
  assert.ok(order.indexOf('api') < order.indexOf('runners'), 'a runner registers with the API')
  assert.ok(order.indexOf('runners') < order.indexOf('alarms'), 'an alarm watches something that exists')
  assert.equal(order.at(-1), 'alarms', 'nothing waits on an alarm')
})

test('the runner is placed directly, because it is a machine rather than a task', () => {
  const { providers, seen } = bundle()
  deployStack({ providers, config, inputs: inputs() })
  // No host: `cluster.ts` says why, and a runner handed one would mean a
  // cluster with no tasks in it on AWS and nothing at all on GCP.
  assert.equal(seen.runners.input.host, undefined)
  assert.ok(seen.runners.input.network, 'it takes its placement from the network instead')
})

test('the stack’s own names win over a stale copy in the store', () => {
  const { providers, seen } = bundle()
  deployStack({
    providers,
    config,
    inputs: inputs({ apiEnvironment: { DB_HOST: 'yesterdays-database.invalid', OIDC_AUDIENCE: 'boxlite' } }),
  })
  const environment = seen.api.request.environment
  assert.equal(read(environment.DB_HOST), 'db.internal', 'the database this deploy built, not the one the store remembers')
  assert.equal(environment.OIDC_AUDIENCE, 'boxlite', 'and a name only the store decides is carried through')
})

test('a name the store delivers as an address and the stack also carries is refused', () => {
  // Two entries for one variable: an ECS task takes the last one written and a
  // Cloud Run revision refuses the pair, so the divergence is refused here
  // where every channel is known.
  const { providers } = bundle()
  assert.throws(
    () => deployStack({ providers, config, inputs: inputs({ apiSecrets: { DB_PASSWORD: 'arn:some-secret' } }) }),
    /DB_PASSWORD would reach the API twice/,
  )
  assert.throws(
    () => deployStack({ providers, config, inputs: inputs({ apiSecrets: { DB_HOST: 'arn:some-secret' } }) }),
    /DB_HOST would reach the API twice/,
  )
})

test('each workload reads its own group and no other', () => {
  const { providers, seen } = bundle()
  deployStack({
    providers,
    config,
    inputs: inputs({
      apiSecrets: { OIDC_CLIENT_SECRET: 'arn:api' },
      proxySecrets: { PROXY_API_KEY: 'arn:proxy' },
      collectorSecrets: { OTEL_COLLECTOR_API_KEY: 'arn:otel' },
      runnerSecrets: { DEFAULT_RUNNER_API_KEY: 'arn:runner' },
    }),
  })
  assert.deepEqual(Object.keys(seen.api.request.secrets), ['OIDC_CLIENT_SECRET'])
  assert.deepEqual(Object.keys(seen.edge.request.secrets), ['PROXY_API_KEY'])
  assert.deepEqual(Object.keys(seen.collector.request.secrets), ['OTEL_COLLECTOR_API_KEY'])
  assert.deepEqual(Object.keys(seen.runners.request.secrets), ['DEFAULT_RUNNER_API_KEY'])
})

test('a stage with no telemetry runs the BoxLite exporter alone', () => {
  // A collector told to write to a ClickHouse that does not exist retries every
  // batch forever, so this is not cosmetic.
  const off = bundle({ clickhouse: false })
  deployStack({ providers: off.providers, config, inputs: inputs() })
  assert.equal(off.seen.collector.request.exporters, '[boxlite_exporter]')
  assert.equal(off.seen.api.request.environment.CLICKHOUSE_URL, undefined, 'and the API is told nothing about one')
  assert.ok(
    !off.seen.api.request.capabilities.some((capability: any) => capability.kind === 'read-telemetry'),
    'nor granted a permission it could never use',
  )

  const on = bundle({ clickhouse: true })
  deployStack({ providers: on.providers, config, inputs: inputs() })
  assert.equal(on.seen.collector.request.exporters, '[boxlite_exporter,clickhouse]')
  assert.equal(read(on.seen.api.request.environment.CLICKHOUSE_USERNAME), 'otel_reader', 'the API reads, never writes')
  assert.equal(read(on.seen.collector.request.environment.CLICKHOUSE_USERNAME), 'otel_writer')
})

test('a stage that sends no mail carries no SMTP host, which is what disables it', () => {
  const { providers, seen } = bundle()
  deployStack({ providers, config, inputs: inputs({ senderDomain: null }) })
  assert.equal(seen.api.request.environment.SMTP_HOST, undefined)
  assert.equal(seen.mail.request.senderDomain, null)
})

test('the API is granted exactly the capabilities its storage supports', () => {
  const { providers, seen } = bundle()
  void seen
  deployStack({ providers, config, inputs: inputs() })
  assert.deepEqual(
    seen.api.request.capabilities.map((capability: any) => capability.kind).sort(),
    ['list-own-bucket', 'manage-volume-buckets', 'read-telemetry', 'vend-volume-credentials'],
  )
})

test('what the deploy reports is what a post-deploy check needs to read', () => {
  const { providers } = bundle()
  const outputs = deployStack({ providers, config, inputs: inputs() })
  assert.equal(read(outputs.apiUrl), 'https://dev.boxlite.ai')
  assert.equal(read(outputs.proxyUrl), 'https://box.dev.boxlite.ai')
  assert.equal(read(outputs.databaseId), 'db-1')
  assert.equal(read(outputs.storageName), 'boxlite-dev-storage')
  assert.equal(outputs.runnerIds.length, 1)
  assert.notEqual(outputs.clickHouseId, null)
})

test('a commit whose images were never published fails before anything is built', () => {
  const { providers } = bundle()
  const withoutProxy = { ...providers, images: () => ({ api: out('a'), 'otel-collector': out('c') }) } as StackProviders
  assert.throws(
    () => deployStack({ providers: withoutProxy, config, inputs: inputs() }),
    /mbuild published no proxy image for this commit/,
  )
})
