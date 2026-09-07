import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import {
  addressFor,
  addressesFor,
  artifactRegistryHost,
  assertTag,
  ecrHost,
  ImageAddressError,
  resolveRegistry,
} from '../src/address.ts'
import { BuildConfigError, parseBuildConfig, registryFor } from '../src/config.ts'

const declare = (stages: Record<string, unknown>) =>
  parseBuildConfig(
    '/repo/apps/infra/mbuild.config.json',
    JSON.stringify({
      root: '../..',
      artifacts: {
        console: { dockerfile: 'apps/console/Dockerfile', context: '.' },
        api: { dockerfile: 'apps/api/Dockerfile', context: '.' },
      },
      scan: { blockOn: ['CRITICAL', 'HIGH'], timeoutSeconds: 300 },
      stages,
    }),
  )

const ecrStage = (repository: string) => ({
  registry: { kind: 'ecr', repository, immutableTags: true, scanOnPush: true },
})

const onArtifactRegistry = () =>
  declare({
    dev: {
      registry: {
        kind: 'artifact-registry',
        repository: 'boxlite',
        immutableTags: true,
        scanOnPush: true,
      },
    },
  })

const config = declare({
  dev: ecrStage('boxlite-app-dev'),
  prod: ecrStage('boxlite-app-prod'),
})

/** What mstage declares. mbuild's own file deliberately does not repeat it. */
const REGION = { dev: 'ap-southeast-1', prod: 'us-east-1' }

const SHA = 'a'.repeat(40)
const ACCOUNT = '000000000000'

test('the repository comes from mbuild and the region from mstage', () => {
  // Two files, one key each. mbuild says which repository receives a stage's
  // artifacts; mstage says where that stage lives, and is not copied here.
  const dev = resolveRegistry({ config, stage: 'dev', region: REGION.dev, accountId: ACCOUNT })
  const prod = resolveRegistry({ config, stage: 'prod', region: REGION.prod, accountId: ACCOUNT })
  assert.equal(
    addressFor({ config, registry: dev, artifact: 'api', tag: SHA }),
    `000000000000.dkr.ecr.ap-southeast-1.amazonaws.com/boxlite-app-dev:${SHA}-api`,
  )
  assert.equal(
    addressFor({ config, registry: prod, artifact: 'api', tag: SHA }),
    `000000000000.dkr.ecr.us-east-1.amazonaws.com/boxlite-app-prod:${SHA}-api`,
  )
})

test('the two registry kinds put the artifact in different halves of the address', () => {
  // The whole reason the module exists: a caller concatenating its own string
  // would work against one registry and be silently wrong for the other.
  const gcp = onArtifactRegistry()
  const registry = resolveRegistry({ config: gcp, stage: 'dev', region: 'asia-southeast1', project: 'boxlite' })
  assert.equal(
    addressFor({ config: gcp, registry, artifact: 'api', tag: SHA }),
    `asia-southeast1-docker.pkg.dev/boxlite/boxlite/api:${SHA}`,
  )
})

test('a stage the file does not declare is a typo, not a new environment', () => {
  // Publishing into an undeclared repository would create it, and nothing
  // would ever pull from it.
  assert.throws(() => registryFor(config, 'staging'), BuildConfigError)
  assert.throws(() => registryFor(config, 'staging'), /declares no stage "staging"\. Declared: dev, prod/)
})

test('each kind is refused the coordinates it cannot use', () => {
  assert.throws(() => resolveRegistry({ config, stage: 'dev', region: REGION.dev }), /needs an account id/)
  assert.throws(
    () => resolveRegistry({ config: onArtifactRegistry(), stage: 'dev', region: 'asia-southeast1' }),
    /needs a project/,
  )
})

test('a stage mstage gives no region cannot be addressed', () => {
  // The failure this prevents is an address with an empty segment, which points
  // at a registry that does not exist and reads almost like one that does.
  assert.throws(
    () => resolveRegistry({ config, stage: 'dev', region: '  ', accountId: ACCOUNT }),
    /mstage declares where a stage lives/,
  )
})

test('a tag that is not one full commit SHA is refused', () => {
  // A deploy names exact bytes; `latest` or a short sha would break that, and
  // an immutable tag cannot be repointed to fix it afterwards.
  assert.throws(() => assertTag('latest'), ImageAddressError)
  assert.throws(() => assertTag(SHA.slice(0, 7)), /one full lowercase commit SHA/)
  assert.throws(() => assertTag(SHA.toUpperCase()), /one full lowercase commit SHA/)
  assert.equal(assertTag(SHA), SHA)
})

test('an artifact the config does not declare cannot be addressed', () => {
  const registry = resolveRegistry({ config, stage: 'dev', region: REGION.dev, accountId: ACCOUNT })
  assert.throws(
    () => addressFor({ config, registry, artifact: 'worker', tag: SHA }),
    /declares no artifact "worker"\. Declared: console, api/,
  )
})

test('every declared artifact is addressable at one commit', () => {
  const registry = resolveRegistry({ config, stage: 'dev', region: REGION.dev, accountId: ACCOUNT })
  assert.deepEqual(Object.keys(addressesFor({ config, registry, tag: SHA })), ['console', 'api'])
})

test('the hosts are built here, not at the call sites that used to', () => {
  assert.equal(
    ecrHost({ accountId: ACCOUNT, region: 'ap-southeast-1' }),
    `${ACCOUNT}.dkr.ecr.ap-southeast-1.amazonaws.com`,
  )
  assert.throws(() => ecrHost({ accountId: 'boxlite', region: 'ap-southeast-1' }), /twelve digits/)
  assert.equal(artifactRegistryHost('asia-southeast1'), 'asia-southeast1-docker.pkg.dev')
})

test('the repository file declares the artifacts that are actually built', () => {
  // Not a fixture: the real file, so an artifact added to one without the other
  // is caught here rather than at the first deploy that cannot find its image.
  const path = new URL('../../mbuild.config.json', import.meta.url)
  const real = parseBuildConfig(path.pathname, readFileSync(path, 'utf8'))
  assert.deepEqual(Object.keys(real.artifacts).sort(), ['api', 'otel-collector', 'proxy'])
  assert.equal(real.artifacts.api!.dockerfile, 'apps/api/Dockerfile')
  assert.equal(real.artifacts.proxy!.dockerfile, 'apps/proxy/Dockerfile')
  assert.equal(real.artifacts['otel-collector']!.dockerfile, 'apps/otel-collector/Dockerfile')
  assert.deepEqual(real.scan.blockOn, ['CRITICAL', 'HIGH'])
  // Every stage is publishable, or a promotion has nowhere to go.
  assert.deepEqual(Object.keys(real.stages).sort(), ['dev', 'gcp-dev', 'prod'])
  // And no stage repeats what mstage already declares.
  assert.equal('region' in registryFor(real, 'dev'), false, 'the region belongs to mstage.config.json')
  assert.notEqual(
    registryFor(real, 'dev').repository,
    registryFor(real, 'prod').repository,
    'one repository for two stages would make promoting a no-op',
  )
})

test('every stage a stage-config file declares can be published into', () => {
  // The two files are read by different tools and neither validates the other,
  // so a stage added to mstage's file alone deploys and then cannot pull: the
  // failure lands on a task that has already been created. Held together here.
  const buildPath = new URL('../../mbuild.config.json', import.meta.url)
  const stagePath = new URL('../../mstage.config.json', import.meta.url)
  const built = parseBuildConfig(buildPath.pathname, readFileSync(buildPath, 'utf8'))
  const staged = JSON.parse(readFileSync(stagePath, 'utf8')) as { stages: Record<string, unknown> }
  assert.deepEqual(Object.keys(built.stages).sort(), Object.keys(staged.stages).sort())
})

test('a stage that lives in GCP publishes to a GCP registry', () => {
  // The two are one decision. An `ecr` repository declared for a stage whose
  // workloads are Cloud Run services is an address nothing in that project can
  // pull, and the deploy that finds out has already built a network.
  const buildPath = new URL('../../mbuild.config.json', import.meta.url)
  const stagePath = new URL('../../mstage.config.json', import.meta.url)
  const built = parseBuildConfig(buildPath.pathname, readFileSync(buildPath, 'utf8'))
  const staged = JSON.parse(readFileSync(stagePath, 'utf8')) as {
    home: string
    stages: Record<string, { home?: string }>
  }
  const expected: Record<string, string> = { aws: 'ecr', gcp: 'artifact-registry' }
  for (const [name, stage] of Object.entries(staged.stages)) {
    assert.equal(
      registryFor(built, name).kind,
      expected[stage.home ?? staged.home],
      `stage ${name} publishes to the wrong kind of registry for the cloud it lives in`,
    )
  }
})
