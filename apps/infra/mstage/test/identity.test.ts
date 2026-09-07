import assert from 'node:assert/strict'
import test from 'node:test'
import { resolveIdentity } from '../src/aws/identity.ts'

const baseScope = {
  stage: 'dev',
  app: 'boxlite',
  region: 'ap-southeast-1',
  roleArn: null,
  roleSessionName: 'mstage',
  project: null,
}

// The real client resolves credentials while sending, so a provider that cannot
// produce them surfaces through send(). The fake keeps that order.
const sts = (answer: any) => (config: any) => ({
  send: async () => {
    await config.credentials()
    return answer
  },
  destroy() {},
})

const credentials =
  (extra = {}) =>
  () =>
  async () => ({ accessKeyId: 'AKIA', secretAccessKey: 'secret', ...extra })

const identity = (scope: any, overrides?: any) =>
  resolveIdentity({
    scope: { ...baseScope, ...scope },
    createSts: sts({ Account: '000000000000', Arn: 'arn:aws:iam::000000000000:user/xinyu' }),
    credentialsFor: credentials(),
    ...overrides,
  })

test('whoami reports the caller and is resolved only once', async () => {
  let calls = 0
  const subject = identity(
    {},
    {
      createSts: (config: any) => ({
        send: async () => {
          await config.credentials()
          calls += 1
          return { Account: '000000000000', Arn: 'arn:aws:iam::000000000000:user/xinyu', UserId: 'AIDA' }
        },
        destroy() {},
      }),
    },
  )
  assert.equal((await subject.whoami()).tenant, '000000000000')
  assert.equal((await subject.whoami()).principal, 'arn:aws:iam::000000000000:user/xinyu')
  assert.equal(calls, 1)
})

test('the SDK failure reaches the caller untranslated', async () => {
  const error = Object.assign(new Error('Could not load credentials from any providers'), {
    name: 'CredentialsProviderError',
  })
  const subject = identity(
    {},
    {
      credentialsFor: () => async () => {
        throw error
      },
    },
  )
  assert.equal(await subject.whoami().catch((thrown: any) => thrown), error)
})

test('credentials expiring inside the requested window fail before the work starts', async () => {
  const now = () => new Date('2026-01-01T00:00:00Z')
  const soon = new Date('2026-01-01T00:04:00Z')
  const subject = identity({}, { credentialsFor: credentials({ expiration: soon }) })

  assert.equal((await subject.expiresAt())!.toISOString(), soon.toISOString())
  await assert.rejects(() => subject.assertUsableFor(600, now), /expire in 240s, less than the 600s/)
  await subject.assertUsableFor(120, now)
})

test('credentials without an expiry are usable for any window', async () => {
  const subject = identity({})
  assert.equal(await subject.expiresAt(), null)
  await subject.assertUsableFor(86_400)
})
