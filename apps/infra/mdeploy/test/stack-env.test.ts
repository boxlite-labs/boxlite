import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { StackEnvError, readStackEnvironment } from '../src/stack-env.ts'
import type { GroupDeclaration } from '../src/env.ts'

const SHA = 'a'.repeat(40)
const DIGEST = 'b'.repeat(64)

const declaration: GroupDeclaration = {
  groups: { deploy: [], api: ['OIDC_CLIENT_ID'], proxy: ['PROXY_API_KEY'], 'otel-collector': [], runner: [] },
  where: '/repo/mstage.config.json',
}

const complete = {
  BOXLITE_IMAGE_TAG: SHA,
  STACK_DOMAIN: 'dev.boxlite.ai',
  PROXY_DOMAIN: 'box.dev.boxlite.ai',
  CLOUDFLARE_ZONE_ID: 'zone-1',
  BOXLITE_RUNNER_BINARY_URL: 'https://example.invalid/runner.tar.gz',
  BOXLITE_RUNNER_BINARY_SHA256: DIGEST,
  OIDC_ISSUER_BASE_URL: 'https://auth.dev.boxlite.ai',
  OIDC_CLIENT_ID: 'client-1',
  PROXY_API_KEY: 'proxy-key',
}

const read = (environment: Record<string, string | undefined>) =>
  readStackEnvironment({ environment, declaration, stage: 'dev', region: 'ap-southeast-1', home: 'aws' })

test('a complete environment resolves everything one deploy decides', () => {
  const resolved = read(complete)
  assert.equal(resolved.tag, SHA)
  assert.equal(resolved.domain, 'dev.boxlite.ai')
  assert.equal(resolved.proxyProtocol, 'http', 'a default, because the hop is inside the network')
  assert.equal(resolved.senderDomain, null, 'a stage that names none sends no mail')
  assert.deepEqual(resolved.runnerFleet, [
    { resourceName: 'Runner', nameTag: 'boxlite-runner-default', controlPlaneRunnerName: 'default' },
  ])
  assert.equal(resolved.runnerBinary.source, 'release')
  assert.deepEqual(resolved.proxySecrets, {})
  assert.deepEqual(resolved.proxyEnvironment, { PROXY_API_KEY: 'proxy-key' })
})

test('a key nothing supplies stops the deploy, named, before anything is built', () => {
  for (const key of ['BOXLITE_IMAGE_TAG', 'STACK_DOMAIN', 'PROXY_DOMAIN', 'CLOUDFLARE_ZONE_ID']) {
    assert.throws(() => read({ ...complete, [key]: undefined }), new RegExp(`${key} is required`), `${key} is not required`)
  }
})

test('a tag that is not one full commit sha is refused', () => {
  // A deploy names exact bytes. A branch name or a short sha would resolve to
  // an address nothing published, and the failure would land on a task that
  // cannot pull — after the apply had already created resources.
  assert.throws(() => read({ ...complete, BOXLITE_IMAGE_TAG: 'main' }), /must be one full lowercase commit SHA/)
  assert.throws(() => read({ ...complete, BOXLITE_IMAGE_TAG: SHA.toUpperCase() }), /lowercase/)
})

test('a runner binary arrives with the checksum that proves it, or not at all', () => {
  // A URL with no checksum is a host that installs whatever answered, which is
  // the one thing a checksum exists to prevent.
  assert.throws(
    () => read({ ...complete, BOXLITE_RUNNER_BINARY_SHA256: undefined }),
    /BOXLITE_RUNNER_BINARY_SHA256 is required/,
  )
  assert.throws(() => read({ ...complete, BOXLITE_RUNNER_BINARY_SHA256: 'not-a-digest' }), /lowercase hex SHA-256/)
  assert.throws(() => read({ ...complete, BOXLITE_RUNNER_BINARY_SOURCE: 'staging' }), /must be "release" or "build"/)
})

test('the fleet keeps the first host’s resource name, whatever it is called', () => {
  // The resource name is what a targeted deploy selects on and what an existing
  // stage's state already calls its first host: renaming it would replace a
  // machine rather than update one.
  const fleet = read({ ...complete, RUNNERS: '3', DEFAULT_RUNNER_NAME: 'sg-1' }).runnerFleet
  assert.deepEqual(
    fleet.map((slot) => [slot.resourceName, slot.controlPlaneRunnerName]),
    [
      ['Runner', 'sg-1'],
      ['Runner-runner-2', 'runner-2'],
      ['Runner-runner-3', 'runner-3'],
    ],
  )
})

test('a fleet size this repository will not create is refused', () => {
  assert.throws(() => read({ ...complete, RUNNERS: '0' }), /must be a whole number from 1 to 100/)
  assert.throws(() => read({ ...complete, RUNNERS: '101' }), /from 1 to 100/)
  assert.throws(() => read({ ...complete, DEFAULT_RUNNER_NAME: 'a' }), /2-255 characters/)
})

test('a managed ClickHouse arrives whole or not at all', () => {
  assert.equal(read(complete).managedClickHouse, null)
  assert.throws(() => read({ ...complete, CLICKHOUSE_URL: 'https://clickhouse.invalid' }), /must be set together/)
  const managed = read({
    ...complete,
    CLICKHOUSE_URL: 'https://clickhouse.invalid',
    CLICKHOUSE_WRITER_PASSWORD_SECRET_ARN: 'arn:writer',
    CLICKHOUSE_READER_PASSWORD_SECRET_ARN: 'arn:reader',
  }).managedClickHouse
  assert.equal(managed?.url, 'https://clickhouse.invalid')
})

test('a service group the deploy never received stops it rather than shipping a short environment', () => {
  // A container handed a silently short environment refuses a feature hours
  // later, somewhere that does not mention the key.
  assert.throws(() => read({ ...complete, OIDC_CLIENT_ID: undefined }), /OIDC_CLIENT_ID must reach the deploy/)
})

test('StackEnvError is the single failure type callers can catch', () => {
  assert.throws(
    () => read({ ...complete, BOXLITE_IMAGE_TAG: 'main' }),
    (error) => error instanceof StackEnvError,
  )
})

/*
 * One copy of the assembly, not two.
 *
 * Both engines call `readStackEnvironment`. Two copies that drifted would
 * deploy a control plane behaving differently on one cloud — the kind of
 * difference found months later, with a matching digest and nothing reporting
 * it. Asserted by reading the two engine entry points rather than by trusting
 * that nobody will paste one into the other.
 */
test('both engines read this one function rather than each reading the environment', () => {
  const entries = ['../sst.config.ts', '../pulumi/program.ts'].map((path) =>
    readFileSync(fileURLToPath(new URL(path, import.meta.url)), 'utf8'),
  )
  for (const source of entries) {
    assert.match(source, /readStackEnvironment\(/, 'an engine that assembled its own would be the drift this prevents')
  }
  // And neither reaches past it for a key the assembly already owns.
  for (const source of entries) {
    assert.doesNotMatch(source, /process\.env\.BOXLITE_IMAGE_TAG/)
    assert.doesNotMatch(source, /process\.env\.STACK_DOMAIN/)
  }
})
