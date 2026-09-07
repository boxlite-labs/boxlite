/*
 * The GCP identity, and the three places it deliberately answers differently
 * from the AWS one.
 *
 * Two of them are the kind of difference that would be papered over by a badly
 * drawn interface: a deadline that does not exist, and a child environment that
 * is a file path rather than a key. The third is where the tenant comes from —
 * the auth library answers it locally, with no call the AWS side's STS makes.
 */

import assert from 'node:assert/strict'
import test from 'node:test'
import { resolveGcpIdentity, type GoogleAuth } from '../src/gcp/identity.ts'
import type { Scope } from '../src/aws/precedence.ts'

const scope = (overrides: Partial<Scope> = {}): Scope =>
  ({
    stage: 'dev',
    protect: false,
    project: 'boxlite-dev',
    app: 'boxlite',
    region: 'asia-southeast1',
    regionSource: 'test',
    roleArn: null,
    roleArnSource: null,
    roleSessionName: null,
    ...overrides,
  }) as Scope

const auth = (project = 'boxlite-dev', email = 'deploy@boxlite-dev.iam.gserviceaccount.com'): GoogleAuth => ({
  async getProjectId() {
    return project
  },
  async getCredentials() {
    return { client_email: email }
  },
})

test('the tenant is the project the auth library is pointed at', async () => {
  const identity = resolveGcpIdentity({ scope: scope(), auth: auth() })
  assert.deepEqual(await identity.whoami(), {
    tenant: 'boxlite-dev',
    principal: 'deploy@boxlite-dev.iam.gserviceaccount.com',
  })
})

test('nothing expires, so the deadline guard has nothing to refuse', async () => {
  // Application Default Credentials refresh themselves. Reporting a deadline
  // would be a promise this cannot keep; refusing on one would be a guard that
  // never fires correctly.
  const identity = resolveGcpIdentity({ scope: scope(), auth: auth() })
  assert.equal(await identity.expiresAt(), null)
  await assert.doesNotReject(() => identity.assertUsableFor(20 * 60))
})

test('a child is given the project, not a key', async () => {
  const identity = resolveGcpIdentity({ scope: scope(), auth: auth() })
  const { env, expiresAt } = await identity.childEnvironment({})
  assert.equal(env.GOOGLE_CLOUD_PROJECT, 'boxlite-dev')
  assert.equal(env.CLOUDSDK_CORE_PROJECT, 'boxlite-dev', 'gcloud and the Pulumi provider read this one')
  assert.equal(env.CLOUDSDK_COMPUTE_REGION, 'asia-southeast1')
  assert.equal(expiresAt, null)
})

test('every AWS variable is cleared, so a child cannot reach the other cloud', async () => {
  // The failure this prevents: a subprocess finds a stale key triple,
  // authenticates to AWS, and fails somewhere that mentions neither cloud.
  const identity = resolveGcpIdentity({ scope: scope(), auth: auth() })
  const { env } = await identity.childEnvironment({
    AWS_PROFILE: 'boxlite',
    AWS_ACCESS_KEY_ID: 'AKIA',
    AWS_SECRET_ACCESS_KEY: 'secret',
    AWS_SESSION_TOKEN: 'token',
    AWS_REGION: 'ap-southeast-1',
    AWS_DEFAULT_REGION: 'ap-southeast-1',
    PATH: '/usr/bin',
  })
  for (const name of [
    'AWS_PROFILE',
    'AWS_ACCESS_KEY_ID',
    'AWS_SECRET_ACCESS_KEY',
    'AWS_SESSION_TOKEN',
    'AWS_REGION',
    'AWS_DEFAULT_REGION',
  ]) {
    assert.equal(env[name], undefined, `${name} reached the child`)
  }
  assert.equal(env.PATH, '/usr/bin', 'everything unrelated is left alone')
})

test('a credentials file is forwarded when one is in use, and invented when it is not', async () => {
  const identity = resolveGcpIdentity({ scope: scope(), auth: auth() })
  const withFile = await identity.childEnvironment({ GOOGLE_APPLICATION_CREDENTIALS: '/tmp/key.json' })
  assert.equal(withFile.env.GOOGLE_APPLICATION_CREDENTIALS, '/tmp/key.json')

  // On a runner with workload identity there is no file; the metadata server is
  // the credential, and naming a path that does not exist would break it.
  const without = await identity.childEnvironment({})
  assert.equal(without.env.GOOGLE_APPLICATION_CREDENTIALS, undefined)
})

test('the project is asked for once, however many times it is used', async () => {
  let calls = 0
  const counting: GoogleAuth = {
    async getProjectId() {
      calls += 1
      return 'boxlite-dev'
    },
    async getCredentials() {
      return {}
    },
  }
  const identity = resolveGcpIdentity({ scope: scope(), auth: counting })
  await identity.whoami()
  await identity.childEnvironment({})
  assert.equal(calls, 1)
})
