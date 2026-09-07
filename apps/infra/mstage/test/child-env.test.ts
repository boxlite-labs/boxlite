import assert from 'node:assert/strict'
import test from 'node:test'
import { childEnvironment, runChild } from '../src/aws/child-env.ts'

const identity = (credentials: any) => ({ credentials: async () => credentials })
const scope = (overrides: any) => ({ region: 'ap-southeast-1', roleArn: null, ...overrides })
const dirty = {
  AWS_PROFILE: 'stale',
  AWS_ACCESS_KEY_ID: 'STALE',
  AWS_SECRET_ACCESS_KEY: 'stale',
  AWS_SESSION_TOKEN: 'stale',
  PATH: '/usr/bin',
}

test('the child receives the resolved credential, never a profile name', async () => {
  // `aws login` writes login_session, which the Go SDK behind SST cannot read.
  // Resolving in JS and passing the triple down is what lets SST run at all.
  const expiration = new Date('2026-01-01T00:00:00Z')
  const { env, expiresAt } = await childEnvironment({
    scope: scope({}),
    identity: identity({ accessKeyId: 'ASIA', secretAccessKey: 's', sessionToken: 't', expiration }),
    base: dirty,
  })
  assert.equal(env.AWS_PROFILE, undefined)
  assert.equal(env.AWS_ACCESS_KEY_ID, 'ASIA')
  assert.equal(env.AWS_SECRET_ACCESS_KEY, 's')
  assert.equal(env.AWS_SESSION_TOKEN, 't')
  assert.equal(expiresAt, expiration)
})

test('an ambient AWS_PROFILE is cleared so the child cannot see two identities', async () => {
  const { env } = await childEnvironment({
    scope: scope({}),
    identity: identity({ accessKeyId: 'AKIA', secretAccessKey: 's' }),
    base: { ...dirty, AWS_PROFILE: 'anything' },
  })
  assert.equal(env.AWS_PROFILE, undefined)
  assert.ok(!(env.AWS_PROFILE && env.AWS_ACCESS_KEY_ID), 'AWS SDK warns when both are set')
})

test('long-lived credentials report no expiry, and no session token is invented', async () => {
  const { env, expiresAt } = await childEnvironment({
    scope: scope({}),
    identity: identity({ accessKeyId: 'AKIA', secretAccessKey: 's' }),
    base: dirty,
  })
  assert.equal(env.AWS_SESSION_TOKEN, undefined)
  assert.equal(expiresAt, null)
})

test('the region is pinned on both variables and the rest of the environment survives', async () => {
  const { env } = await childEnvironment({
    scope: scope({}),
    identity: identity({ accessKeyId: 'AKIA', secretAccessKey: 's' }),
    base: dirty,
  })
  assert.equal(env.AWS_REGION, 'ap-southeast-1')
  assert.equal(env.AWS_DEFAULT_REGION, 'ap-southeast-1')
  assert.equal(env.PATH, '/usr/bin')
})

test('a non-zero child is an error, and a clean one resolves', async () => {
  const fake = (code: number) => () => ({
    on(event: string, handler: (...args: any[]) => void) {
      if (event === 'close') queueMicrotask(() => handler(code, null))
      return this
    },
  })
  assert.equal(await runChild({ command: 'x', args: [], env: {}, spawnProcess: fake(0) }), 0)
  await assert.rejects(() => runChild({ command: 'x', args: [], env: {}, spawnProcess: fake(3) }), /x exited with 3/)
})
