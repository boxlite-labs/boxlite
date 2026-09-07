import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { parseConfig } from 'mstage/config'
import { ApiEnvironmentError, BILLING_KEYS, STATUS_SYNC_KEYS, apiEnvironmentFrom } from '../src/api-environment.ts'
import type { GroupDeclaration } from '../src/env.ts'

const groups = (api: string[], secret?: string[]): GroupDeclaration => ({
  groups: { deploy: [], api, ...(secret ? { secret } : {}) },
  where: '/repo/mstage.config.json',
})

const base = { STACK_DOMAIN: 'dev.boxlite.ai', OIDC_ISSUER_BASE_URL: 'https://auth.dev.boxlite.ai' }

const assemble = (environment: Record<string, string | undefined>, declaration = groups([])) =>
  apiEnvironmentFrom({ environment, declaration, region: 'ap-southeast-1', stage: 'dev', home: 'aws' })

test('the two dashboard origins are different on purpose, and both have a default', () => {
  // Static assets go through the CDN at the root domain; the dashboard's API
  // client talks to api.<domain> directly, because the CDN caps a WebSocket at
  // ten minutes and times an origin read out at sixty seconds.
  const { environment } = assemble(base)
  assert.equal(environment.DASHBOARD_URL, 'https://dev.boxlite.ai')
  assert.equal(environment.DASHBOARD_BASE_API_URL, 'https://api.dev.boxlite.ai')
  assert.notEqual(environment.DASHBOARD_URL, environment.DASHBOARD_BASE_API_URL)

  const overridden = assemble({ ...base, DASHBOARD_URL: 'https://console.example.com' })
  assert.equal(overridden.environment.DASHBOARD_URL, 'https://console.example.com')
})

test('an issuer is required, because a placeholder would let the stack look healthy', () => {
  assert.throws(() => assemble({ STACK_DOMAIN: 'dev.boxlite.ai' }), /OIDC_ISSUER_BASE_URL is required/)
  assert.throws(() => assemble({ OIDC_ISSUER_BASE_URL: 'https://auth' }), /STACK_DOMAIN is required/)
})

test('the management client is refused without the audience it needs', () => {
  // The one flag that is not derived from its credential: the audience is a
  // setting rather than a secret, so a stage that turned the feature on without
  // one would fail at its first account-link instead of at boot.
  assert.throws(
    () => assemble({ ...base, OIDC_MANAGEMENT_API_ENABLED: 'true' }),
    /OIDC_MANAGEMENT_API_AUDIENCE is required/,
  )
  const { environment } = assemble({
    ...base,
    OIDC_MANAGEMENT_API_ENABLED: 'true',
    OIDC_MANAGEMENT_API_AUDIENCE: 'https://auth.dev.boxlite.ai/api/v2/',
  })
  assert.equal(environment.OIDC_MANAGEMENT_API_ENABLED, 'true')
  assert.equal(environment.OIDC_MANAGEMENT_API_AUDIENCE, 'https://auth.dev.boxlite.ai/api/v2/')
})

test('a stage that names no billing service keeps the dashboard’s billing pages gated off', () => {
  const { environment } = assemble(base)
  assert.equal(environment.BILLING_API_URL, undefined)
  assert.equal(environment.USAGE_EXPORT_ENABLED, undefined, 'nothing to enable, so nothing said about it')
})

test('the usage export URL is derived from the billing origin rather than taken twice', () => {
  // The publisher appends /internal/usage-events, which the billing service
  // answers off its bare origin. Sending the billing base path would 404 every
  // batch, and a second setting is a second thing that can be pointed elsewhere.
  const { environment } = assemble(
    { ...base, BILLING_API_URL: 'https://commerce.example.com/api/billing', USAGE_EXPORT_TOKEN: 'token' },
    groups(['USAGE_EXPORT_TOKEN']),
  )
  assert.equal(environment.USAGE_EXPORT_URL, 'https://commerce.example.com')
  assert.equal(environment.USAGE_EXPORT_ENABLED, 'true')
  assert.equal(environment.USAGE_ALLOCATION_SNAPSHOT_ENABLED, 'true')
})

test('export is off until the credential arrives, whichever channel brings it', () => {
  // Derived from presence rather than from the value's text: a credential
  // delivered by reference is an ARN here, and asking whether an ARN is empty
  // would answer the wrong question. The API refuses to boot when export is on
  // without a token, so a stage pointed at a billing service and never given
  // the secret has to come up dark rather than crash-loop.
  const asValue = assemble(
    { ...base, BILLING_API_URL: 'https://commerce.example.com', USAGE_EXPORT_TOKEN: '' },
    groups(['USAGE_EXPORT_TOKEN']),
  )
  assert.equal(asValue.environment.USAGE_EXPORT_ENABLED, 'false')

  const asAddress = assemble(
    {
      ...base,
      BILLING_API_URL: 'https://commerce.example.com',
      // What a marked key holds in the store: an address, as mstage writes one.
      USAGE_EXPORT_TOKEN: JSON.stringify({
        address: 'arn:aws:secretsmanager:ap-southeast-1:123456789012:secret:boxlite/dev/usage-AbCdEf',
      }),
    },
    groups(['USAGE_EXPORT_TOKEN'], ['USAGE_EXPORT_TOKEN']),
  )
  assert.equal(asAddress.environment.USAGE_EXPORT_ENABLED, 'true', 'held is held, whichever channel carries it')
  assert.ok('USAGE_EXPORT_TOKEN' in asAddress.secrets, 'and it travels as an address, never as a value')
  assert.equal(asAddress.environment.USAGE_EXPORT_TOKEN, undefined)
})

test('a billing URL that is not a URL is refused where it is written, not where it is called', () => {
  assert.throws(() => assemble({ ...base, BILLING_API_URL: 'commerce.example.com' }), /must be an absolute URL/)
})

test('the status sync is gated on the alert source, and its own switch follows the credential', () => {
  const off = assemble(base)
  assert.equal(off.environment.INCIDENT_IO_ALERT_SOURCE_CONFIG_ID, undefined)

  // Empty rather than absent: a group names keys the store *must* hold, and an
  // empty value is how a store says "configured, and switched off".
  const dark = assemble(
    { ...base, INCIDENT_IO_ALERT_SOURCE_CONFIG_ID: 'src-1', INCIDENT_IO_TOKEN: '' },
    groups(['INCIDENT_IO_TOKEN']),
  )
  assert.equal(dark.environment.STATUS_SYNC_ENABLED, 'false')
  assert.equal(dark.environment.STATUS_SYNC_DEDUP_PREFIX, 'boxlite-dev', 'the per-stage identity is the stage name')

  const on = assemble(
    { ...base, INCIDENT_IO_ALERT_SOURCE_CONFIG_ID: 'src-1', INCIDENT_IO_TOKEN: 'token' },
    groups(['INCIDENT_IO_TOKEN']),
  )
  assert.equal(on.environment.STATUS_SYNC_ENABLED, 'true')
})

test('the system image registry is all or nothing, gated on the URL', () => {
  const without = assemble(base)
  assert.equal(without.environment.BOXLITE_SYSTEM_SOURCE_REGISTRY_NAME, undefined)

  const { environment } = assemble({ ...base, BOXLITE_SYSTEM_SOURCE_REGISTRY_URL: 'https://ghcr.io' })
  assert.equal(environment.BOXLITE_SYSTEM_SOURCE_REGISTRY_URL, 'https://ghcr.io')
  assert.equal(environment.BOXLITE_SYSTEM_SOURCE_REGISTRY_NAME, 'BoxLite System Source Registry')
})

test('a flag that is neither true nor false is refused here rather than at the service’s boot', () => {
  assert.throws(() => assemble({ ...base, OIDC_MANAGEMENT_API_ENABLED: 'yes' }), /must be true or false/)
  assert.throws(
    () => assemble({ ...base, OIDC_MANAGEMENT_API_ENABLED: 'yes' }),
    (error) => error instanceof ApiEnvironmentError,
  )
})

test('every key these gates expect is one the store is declared to fetch', () => {
  // A name this module reads but no group names would read as "not configured"
  // on every deploy, with nothing saying why.
  //
  // Read through mstage's own parser rather than off the raw JSON: a group may
  // be written as a bare array or as required/optional, and a test that
  // flattened only one shape would silently stop checking the other.
  const path = new URL('../../mstage.config.json', import.meta.url)
  const declared = parseConfig(path.pathname, readFileSync(path, 'utf8'))
  const fetched = new Set(Object.values(declared.envSelectGroup).flat())
  for (const key of [...BILLING_KEYS, ...STATUS_SYNC_KEYS]) {
    assert.ok(fetched.has(key), `${key} is read by api-environment.ts but no group in mstage.config.json fetches it`)
  }
})

test('nothing a single deploy decides is demanded of the store', () => {
  // An image tag and a runner binary's checksum are different on every run, so
  // a store can never hold them: naming them in a group makes every deploy fail
  // on a key nobody could have seeded. They reach both engines through the
  // process environment, which is where the workflow puts them.
  const path = new URL('../../mstage.config.json', import.meta.url)
  const declared = parseConfig(path.pathname, readFileSync(path, 'utf8'))
  const named = new Set(Object.values(declared.envSelectGroup).flat())
  for (const key of [
    'BOXLITE_IMAGE_TAG',
    'BOXLITE_RUNNER_BINARY_URL',
    'BOXLITE_RUNNER_BINARY_SHA256',
    'BOXLITE_RUNNER_BINARY_SOURCE',
  ]) {
    assert.ok(!named.has(key), `${key} is decided per deploy and cannot live in the store`)
  }
})

test('the deploy group demands only what a deploy cannot run without', () => {
  // The rest is optional, and that split is the whole point: a required list
  // that swept in every feature flag would force a stage to seed forty empty
  // strings to say nothing at all.
  const path = new URL('../../mstage.config.json', import.meta.url)
  const declared = parseConfig(path.pathname, readFileSync(path, 'utf8'))
  assert.deepEqual(
    declared.envSelectGroup.deploy!.filter((key) => !declared.envOptional.deploy!.includes(key)).sort(),
    [
      'BOXLITE_STAGE_CONFIG_DIGEST',
      'CLOUDFLARE_API_TOKEN',
      'CLOUDFLARE_DEFAULT_ACCOUNT_ID',
      'CLOUDFLARE_ZONE_ID',
      'IAM_PERMISSIONS_BOUNDARY_STAGE',
      'OIDC_ISSUER_BASE_URL',
      'PROXY_DOMAIN',
      'STACK_DOMAIN',
    ],
  )
})
