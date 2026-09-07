import assert from 'node:assert/strict'
import test from 'node:test'
import { parseConfig } from '../src/config/load.ts'
import { ScopeError, resolveScope } from '../src/aws/precedence.ts'

const config = parseConfig(
  '/repo/mstage.config.json',
  JSON.stringify({
    app: 'boxlite',
    home: 'aws',
    stages: {
      dev: { region: 'ap-southeast-1' },
      prod: { region: 'us-east-1', project: 'boxlite-prod', protect: true },
      'gcp-dev': { region: 'asia-southeast1', home: 'gcp', project: 'boxlite-gcp-dev' },
      unbound: {},
    },
  }),
)

const scope = (options: any, environment: any = {}) => resolveScope({ options, config, environment })

test('a declared stage carries its project, protection and app through', () => {
  const resolved = scope({ stage: 'dev' })
  assert.equal(resolved.stage, 'dev')
  assert.equal(resolved.project, null)
  assert.equal(resolved.protect, false)
  assert.equal(resolved.app, 'boxlite')
  assert.equal(scope({ stage: 'prod' }).protect, true)
  assert.equal(scope({ stage: 'prod' }).project, 'boxlite-prod')
})

test('the cloud is folded onto the scope once, so nothing downstream reads the file again', () => {
  // Two stages of one repository, on two clouds. Everything that follows — the
  // store backend, the identity, the engine — reads this one field.
  assert.equal(scope({ stage: 'dev' }).home, 'aws')
  assert.equal(scope({ stage: 'gcp-dev' }).home, 'gcp')
})

test('a stage the config never declared is refused with the list of real ones', () => {
  assert.throws(() => scope({ stage: 'dve' }), /Stage "dve" \(from --stage\) is not declared/)
  assert.throws(() => scope({ stage: 'dve' }), /Declared stages: dev, prod, gcp-dev, unbound/)
})

test('a missing stage is an error, never a default', () => {
  assert.throws(() => scope({}), /--stage is required/)
  assert.equal(scope({}, { MSTAGE_STAGE: 'dev' }).stageSource, 'MSTAGE_STAGE')
})

test("the stage's declared region outranks an ambient AWS_REGION", () => {
  const resolved = scope({ stage: 'prod' }, { AWS_REGION: 'ap-southeast-1' })
  assert.equal(resolved.region, 'us-east-1')
  assert.equal(resolved.regionSource, 'prod in mstage.config.json')
})

test('--region is the only thing that overrides a declared region', () => {
  const resolved = scope({ stage: 'prod', region: 'eu-west-1' }, { AWS_REGION: 'ap-southeast-1' })
  assert.equal(resolved.region, 'eu-west-1')
  assert.equal(resolved.regionSource, '--region')
})

test('an undeclared region falls through to the environment', () => {
  assert.equal(scope({ stage: 'unbound' }, { AWS_REGION: 'eu-west-1' }).region, 'eu-west-1')
  assert.equal(scope({ stage: 'unbound' }, { AWS_DEFAULT_REGION: 'eu-west-2' }).region, 'eu-west-2')
})

test('an unresolvable region fails instead of becoming us-east-1', () => {
  assert.throws(() => scope({ stage: 'unbound' }), ScopeError)
  assert.throws(() => scope({ stage: 'unbound' }), /Could not determine an AWS region for stage "unbound"/)
})

test('a role is only assumed when one is named, and carries a session name', async () => {
  assert.equal(scope({ stage: 'dev' }).roleArn, null)
  const assumed = scope({ stage: 'dev', 'role-arn': 'arn:aws:iam::000000000000:role/deploy' })
  assert.equal(assumed.roleArn, 'arn:aws:iam::000000000000:role/deploy')
  assert.equal(assumed.roleSessionName, 'mstage')
  const named = scope({ stage: 'dev', 'role-arn': 'arn:x', 'role-session-name': 'deploy-42' })
  assert.equal(named.roleSessionName, 'deploy-42')
})

test('--app overrides the config, and MSTAGE_APP sits between them', async () => {
  assert.equal(scope({ stage: 'dev', app: 'boxlite' }).app, 'boxlite')
  assert.equal(scope({ stage: 'dev' }, { MSTAGE_APP: 'boxlite' }).app, 'boxlite')
  assert.equal(scope({ stage: 'dev' }).appSource, '/repo/mstage.config.json')
})

test('the scope has no opinion about which credentials to use', async () => {
  // Choosing is the SDK chain's job; mstage only verifies the account it lands on.
  const resolved = scope({ stage: 'dev' }, { AWS_PROFILE: 'anything' })
  assert.ok(!('profile' in resolved), 'a resolved scope must not name a profile')
  assert.ok(!('profileSource' in resolved))
})
