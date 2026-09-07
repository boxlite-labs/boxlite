import assert from 'node:assert/strict'
import test from 'node:test'
import { EnvError } from '../src/env/backend.ts'
import { SECRET_GROUP, assertSecretAddresses, secretAddressesOf } from '../src/env/secret-address.ts'

const PARAMETER = 'arn:aws:ssm:ap-southeast-1:123456789012:parameter/boxlite/dev/oidc-client-secret'
const SECRETS_MANAGER = 'arn:aws:secretsmanager:ap-southeast-1:123456789012:secret:boxlite/dev/key-AbCdEf'
const SECRET_MANAGER = 'projects/boxlite-dev/secrets/oidc-client-secret'

const held = (address: string) => JSON.stringify({ address })

const groups = { [SECRET_GROUP]: ['OIDC_CLIENT_SECRET', 'ADMIN_API_KEY'] }

test('a group of stored documents comes back as the addresses they hold', () => {
  assert.deepEqual(
    secretAddressesOf({
      values: { OIDC_CLIENT_SECRET: held(PARAMETER), ADMIN_API_KEY: held(SECRETS_MANAGER) },
      home: 'aws',
    }),
    { OIDC_CLIENT_SECRET: PARAMETER, ADMIN_API_KEY: SECRETS_MANAGER },
  )
  assert.deepEqual(secretAddressesOf({ values: { KEY: held(SECRET_MANAGER) }, home: 'gcp' }), {
    KEY: SECRET_MANAGER,
  })
})

test('both AWS forms are addresses, because both are what an ECS secrets entry resolves', () => {
  // The platform's existing secrets live in Secrets Manager (apps/infra/README.md);
  // which of the two services holds one is not the store's question to answer.
  for (const address of [PARAMETER, SECRETS_MANAGER]) {
    assert.deepEqual(secretAddressesOf({ values: { KEY: held(address) }, home: 'aws' }), { KEY: address })
  }
})

test('an address for the other cloud is refused, so a store cannot point nowhere', () => {
  assert.throws(() => secretAddressesOf({ values: { KEY: held(SECRET_MANAGER) }, home: 'aws' }), EnvError)
  assert.throws(
    () => secretAddressesOf({ values: { KEY: held(PARAMETER) }, home: 'gcp' }),
    /KEY does not name a Secret Manager secret name/,
  )
})

test('a bare parameter name is not an address; an ARN is what names the region and the account', () => {
  assert.throws(
    () => secretAddressesOf({ values: { KEY: held('/boxlite/dev/oidc-client-secret') }, home: 'aws' }),
    /KEY does not name a Parameter Store parameter ARN/,
  )
})

test('a version on the end is refused, because Cloud Run takes the version itself', () => {
  assert.throws(
    () => secretAddressesOf({ values: { KEY: held(`${SECRET_MANAGER}/versions/3`) }, home: 'gcp' }),
    /with no version on the end/,
  )
})

test('a document that is not one address, spelled exactly, says what is wrong with it', () => {
  const refused = (value: string) => () => secretAddressesOf({ values: { KEY: value }, home: 'aws' })

  assert.throws(refused('not json at all'), /KEY does not hold JSON/)
  assert.throws(refused(JSON.stringify([PARAMETER])), /KEY does not hold a JSON object/)
  assert.throws(refused(JSON.stringify({ arn: PARAMETER })), /KEY names arn, which an address has no field for/)
  assert.throws(refused(JSON.stringify({ address: PARAMETER, version: '3' })), /KEY names version/)
  assert.throws(refused(JSON.stringify({})), /KEY has no "address"/)
  assert.throws(refused(JSON.stringify({ address: '   ' })), /KEY has no "address"/)
  assert.throws(refused(JSON.stringify({ address: 3 })), /KEY has no "address"/)
})

test('no refusal quotes the value, because the mistake it catches is the secret itself', () => {
  // Field names are named — a typo like `arn` for `address` is only actionable
  // if the message says which field it was. Values never are, and the two shapes
  // below are the ones a secret arrives in: written bare, or written as the
  // address.
  for (const value of ['hunter2', JSON.stringify({ address: 'hunter2' })]) {
    assert.throws(
      () => secretAddressesOf({ values: { KEY: value }, home: 'aws' }),
      (error: Error) => {
        assert.ok(!error.message.includes('hunter2'), `refusing ${value} put it in the message`)
        return true
      },
    )
  }
})

test('a write is checked for the keys the group names, and for no others', () => {
  assert.throws(
    () => assertSecretAddresses({ entries: [['OIDC_CLIENT_SECRET', 'hunter2']], groups, home: 'aws' }),
    /OIDC_CLIENT_SECRET does not hold JSON/,
  )
  assertSecretAddresses({ entries: [['SOMETHING_ELSE', 'hunter2']], groups, home: 'aws' })
  assertSecretAddresses({ entries: [['OIDC_CLIENT_SECRET', held(PARAMETER)]], groups, home: 'aws' })
})

test('a repository that declares no such group has nothing to check, on any home', () => {
  assertSecretAddresses({ entries: [['ANY', 'hunter2']], groups: { deploy: ['ANY'] }, home: 'aws' })
  assertSecretAddresses({ entries: [['ANY', 'hunter2']], groups: {}, home: 'gcp' })
})
