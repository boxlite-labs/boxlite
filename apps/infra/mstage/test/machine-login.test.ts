import assert from 'node:assert/strict'
import test from 'node:test'
import { PROVIDER_TOOLS, signInWithClientCredentials } from '../src/auth/sessions.ts'

const CREDENTIALS = {
  domain: 'dev-example.us.auth0.com',
  clientId: 'client-id',
  clientSecret: 'super-secret-value',
}

const spy = (result: any) => {
  const calls: any[] = []
  return {
    calls,
    runCommand: (command: string, args: string[], options: any) => {
      calls.push({ command, args, options })
      return { status: 0, stdout: '', stderr: '', ...result }
    },
  }
}

test('auth0 machine login passes the tenant and client through the documented flags', () => {
  const { runCommand, calls } = spy({})
  assert.deepEqual(signInWithClientCredentials('auth0', CREDENTIALS, runCommand), { ok: true })
  assert.equal(calls[0].command, 'auth0')
  assert.deepEqual(calls[0].args, [
    'login',
    '--domain',
    'dev-example.us.auth0.com',
    '--client-id',
    'client-id',
    '--client-secret',
    'super-secret-value',
    '--no-input',
    '--no-color',
  ])
})

test('a machine login is piped, not inherited, and cannot hang the caller', () => {
  const { runCommand, calls } = spy({})
  signInWithClientCredentials('auth0', CREDENTIALS, runCommand)
  assert.notEqual(calls[0].options.stdio, 'inherit', 'nothing to prompt for; output must be capturable')
  assert.equal(calls[0].options.timeout, 15_000)
})

test('a provider without a machine login says so instead of guessing a command', () => {
  const result = signInWithClientCredentials('github', CREDENTIALS, () => {
    throw new Error('must not run')
  })
  assert.equal(result.ok, false)
  assert.match(result.detail!, /github has no machine login\. Supported: auth0/)
  assert.equal(PROVIDER_TOOLS.github.machineSignIn, undefined)
})

test('a missing field is named without naming its value', () => {
  for (const field of ['domain', 'clientId', 'clientSecret']) {
    const result = signInWithClientCredentials('auth0', { ...CREDENTIALS, [field]: '' }, () => {
      throw new Error('must not run')
    })
    assert.equal(result.ok, false)
    assert.equal(result.detail, `machine login needs a non-empty ${field}`)
  }
})

test('the secret never survives into a failure message', () => {
  // The CLI quotes its arguments back on some failures; that output is reported.
  const { runCommand } = spy({
    status: 1,
    stderr: `failed: auth0 login --client-secret ${CREDENTIALS.clientSecret} rejected`,
  })
  const result = signInWithClientCredentials('auth0', CREDENTIALS, runCommand)
  assert.equal(result.ok, false)
  assert.ok(!result.detail!.includes(CREDENTIALS.clientSecret), result.detail)
  assert.match(result.detail!, /--client-secret \*\*\* rejected/)
})

test('an absent auth0 CLI is reported with its install hint', () => {
  const result = signInWithClientCredentials('auth0', CREDENTIALS, () => ({
    error: Object.assign(new Error('x'), { code: 'ENOENT' }),
  }))
  assert.match(result.detail!, /auth0 is not installed\. Install it with: brew install auth0/)
})
