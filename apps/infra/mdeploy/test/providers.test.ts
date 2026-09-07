/*
 * The two bundles, held against each other.
 *
 * The stack is written against `StackProviders` and names no cloud, so the one
 * thing that could silently diverge is a bundle that answers fewer questions
 * than the other — a module quietly absent on one cloud would be a `TypeError`
 * halfway through an apply, on whichever cloud was left behind.
 *
 * Building a bundle creates no resource: every entry is a function from the
 * modules it depends on to a provider. That is what lets this run with no
 * account, no project and no engine.
 */

import assert from 'node:assert/strict'
import test from 'node:test'
import { awsStackProviders } from '../stack/providers/aws/index.ts'
import { gcpStackProviders } from '../stack/providers/gcp/index.ts'

/**
 * The globals each engine installs, in as much shape as building a bundle
 * reads. Only the AWS bundle touches one while it is being built —
 * `sst.cloudflare.dns()` — which is itself worth pinning: a bundle that
 * created a resource at construction time could not be built before the stack
 * decided the order.
 */
const installGlobals = () => {
  const target = globalThis as Record<string, unknown>
  target.$app = { name: 'boxlite', stage: 'dev' }
  target.$util = { output: (value: unknown) => value, secret: (value: unknown) => value }
  target.sst = { cloudflare: { dns: () => ({ kind: 'cloudflare-dns' }) } }
}
installGlobals()

const aws = () =>
  awsStackProviders({
    stage: 'dev',
    region: 'ap-southeast-1',
    accountId: '123456789012',
    domain: 'dev.boxlite.ai',
    artifactsBucket: 'boxlite-app-dev-artifacts-123456789012',
    managedClickHouse: null,
  })

const gcp = () =>
  gcpStackProviders({
    stage: 'gcp-dev',
    region: 'asia-southeast1',
    project: 'boxlite-gcp-dev',
    domain: 'gcp-dev.boxlite.ai',
    zoneId: 'zone-1',
  })

test('both clouds answer every module the stack asks about', () => {
  // A module absent on one cloud is a TypeError halfway through an apply, on
  // whichever cloud was left behind.
  const expected = [
    'alarms',
    'api',
    'cache',
    'clickhouse',
    'cluster',
    'collector',
    'database',
    'edge',
    'images',
    'mail',
    'network',
    'runners',
    'storage',
  ]
  assert.deepEqual(Object.keys(aws()).sort(), expected)
  assert.deepEqual(Object.keys(gcp()).sort(), expected)
})

test('every entry is a function, so a bundle exists before the first resource does', () => {
  for (const [cloud, bundle] of [
    ['aws', aws()],
    ['gcp', gcp()],
  ] as const) {
    for (const [name, entry] of Object.entries(bundle)) {
      assert.equal(typeof entry, 'function', `${cloud}.${name} is not a function`)
    }
  }
})

test('a bundle handed the other cloud’s network says which module was, by name', () => {
  // The tagged unions are narrowed here and nowhere else, so this is the one
  // place the failure can name the module rather than a field that is missing.
  const foreignNetwork = {
    binding: { cloud: 'gcp' as const },
    placementFor: () => ({ cloud: 'gcp' as const }),
    ready: [],
  } as any
  assert.throws(() => aws().database({ network: foreignNetwork }), /The AWS stack was handed gcp network/)
  assert.throws(() => aws().cache({ network: foreignNetwork }), /The AWS stack was handed gcp network/)

  const awsNetwork = {
    binding: { cloud: 'aws' as const },
    placementFor: () => ({ cloud: 'aws' as const }),
    ready: [],
  } as any
  assert.throws(() => gcp().database({ network: awsNetwork }), /The GCP stack was handed aws network/)
  // The GCP proxy is a machine rather than a Cloud Run service, so its bundle
  // ignores the host and narrows the placement instead — which is where the
  // wrong cloud surfaces.
  const gcpHost = { cloud: 'gcp' as const, region: 'asia-southeast1' }
  assert.throws(
    () => gcp().edge({ host: gcpHost, network: awsNetwork, dependsOn: [] }),
    /The GCP stack was handed aws placement/,
  )
})

test('a bundle handed the other cloud’s host says so too', () => {
  const gcpHost = { cloud: 'gcp' as const, region: 'asia-southeast1' } as const
  assert.throws(
    () => aws().collector({ host: gcpHost as any, network: {} as any, clickhouse: {} as any, dependsOn: [] }),
    /The AWS stack was handed gcp host/,
  )
})
