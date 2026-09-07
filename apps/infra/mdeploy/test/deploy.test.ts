/*
 * The one dispatch: which cloud this stage lives in, and what serves it.
 *
 * Driven without either cloud by injecting `resolveHomeWith`, which is what
 * `resolveDeployTarget` takes it for. What is checked is that the switch is
 * exhaustive, that each engine is handed the identity it can actually use, and
 * that the group list is the target's answer rather than the caller's — because
 * those are the four things that used to be separate `=== 'gcp'` checks and had
 * to agree.
 */

import assert from 'node:assert/strict'
import test from 'node:test'
import { resolveDeployTarget } from '../src/deploy.ts'
import { DEPLOY_GROUP, PULUMI_GROUP, SERVICE_GROUPS } from '../src/env.ts'
import { REQUIRED_CREDENTIAL_SECONDS, REQUIRED_PREVIEW_SECONDS, windowFor } from '../src/credential-window.ts'

const config = { root: '/repo/apps/infra', path: '/repo/apps/infra/mstage.config.json' }
const scope = (overrides: Record<string, unknown> = {}) =>
  ({
    stage: 'dev',
    app: 'boxlite',
    region: 'ap-southeast-1',
    home: 'aws',
    project: null,
    protect: false,
    roleArn: null,
    roleArnSource: null,
    roleSessionName: null,
    regionSource: 'test',
    ...overrides,
  }) as any

const identity = (home: string) => ({
  home,
  region: 'ap-southeast-1',
  stage: 'dev',
  app: 'boxlite',
  credentials: async () => ({ accessKeyId: 'a', secretAccessKey: 'b', sessionToken: 'c', expiration: undefined }),
  whoami: async () => ({}),
  expiresAt: async () => null,
  assertUsableFor: async () => {},
  childEnvironment: async () => ({ env: {}, expiresAt: null }),
})

const home = (cloud: 'aws' | 'gcp') => async () => ({
  identity: identity(cloud),
  backend: { home: cloud } as any,
  stateBucket: async () => 'boxlite-state',
})

test('each cloud is answered by its own engine, and by nothing else', async () => {
  const aws = await resolveDeployTarget({ config, scope: scope(), resolveHomeWith: home('aws') as any })
  assert.equal(aws.cloud, 'aws')
  assert.equal(aws.engine, 'sst')

  const gcp = await resolveDeployTarget({
    config,
    scope: scope({ home: 'gcp', project: 'boxlite-gcp-dev' }),
    resolveHomeWith: home('gcp') as any,
  })
  assert.equal(gcp.cloud, 'gcp')
  assert.equal(gcp.engine, 'pulumi')
})

test('the store the target carries is the one that stage’s cloud answered with', async () => {
  // Carried rather than resolved again by the caller: the store, the identity,
  // the engine and the state all differ per cloud, and as four separate checks
  // they were four call sites that had to agree.
  const target = await resolveDeployTarget({
    config,
    scope: scope({ home: 'gcp', project: 'boxlite-gcp-dev' }),
    resolveHomeWith: home('gcp') as any,
  })
  assert.equal(target.backend.home, 'gcp')
})

test('a rollout reads every service’s group; a teardown reads none of them', async () => {
  // A destroyed stage has no service to configure, so asking for a service
  // group would make a half-configured stage unremovable — the one state most
  // likely to need removing.
  const declaration = { envSelectGroup: { deploy: [], api: [], proxy: [] } }
  const target = await resolveDeployTarget({ config, scope: scope(), resolveHomeWith: home('aws') as any })
  const rollout = target.environmentGroups('deploy', declaration as any)
  for (const group of [DEPLOY_GROUP, ...SERVICE_GROUPS]) assert.ok(rollout.includes(group), `${group} is not read`)
  assert.deepEqual(target.environmentGroups('remove', declaration as any), [DEPLOY_GROUP])
})

test('only the Pulumi engine asks for the passphrase, and it asks on a teardown too', async () => {
  // SST discovers its state bucket from a parameter and seals nothing with a
  // passphrase, so there is no engine group to add there. A teardown still has
  // to open the state to destroy what it describes.
  const declaration = { envSelectGroup: { deploy: [], api: [] } }
  const aws = await resolveDeployTarget({ config, scope: scope(), resolveHomeWith: home('aws') as any })
  assert.ok(!aws.environmentGroups('deploy', declaration as any).includes(PULUMI_GROUP))

  const gcp = await resolveDeployTarget({
    config,
    scope: scope({ home: 'gcp', project: 'boxlite-gcp-dev' }),
    resolveHomeWith: home('gcp') as any,
  })
  assert.ok(gcp.environmentGroups('deploy', declaration as any).includes(PULUMI_GROUP))
  assert.ok(gcp.environmentGroups('remove', declaration as any).includes(PULUMI_GROUP))
})

test('a preview does not need the window a rollout does', () => {
  // A preview holds no lock and changes nothing, so refusing to *look* because
  // a session expires in ten minutes would refuse the cheapest thing there is.
  assert.equal(windowFor('diff'), REQUIRED_PREVIEW_SECONDS)
  assert.equal(windowFor('deploy'), REQUIRED_CREDENTIAL_SECONDS)
  assert.equal(windowFor('remove'), REQUIRED_CREDENTIAL_SECONDS, 'a teardown holds the same lock as a rollout')
})

test('targeting is refused on the Pulumi path rather than translated', async () => {
  // `plan.ts` lists SST's logical component names and Pulumi selects on URNs;
  // the GCP bundle's resources are not even named the same. A mapping invented
  // at that seam would deploy some other subset and report success.
  const target = await resolveDeployTarget({
    config,
    scope: scope({ home: 'gcp', project: 'boxlite-gcp-dev' }),
    resolveHomeWith: home('gcp') as any,
  })
  await assert.rejects(
    () => target.run({ targets: ['Vpc'], log: () => {} }),
    /--module is not supported on a GCP stage/,
  )
})
