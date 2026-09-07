import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import {
  DeployConfigError,
  alarmsFor,
  cacheFor,
  clickHouseFor,
  databaseFor,
  parseDeployConfig,
  runnersFor,
  storageFor,
} from '../src/config.ts'

const valid = {
  database: { name: 'boxlite', size: 'small', highlyAvailable: false, backupRetentionDays: 7, protected: false },
  cache: { size: 'small', clustered: false, encryptInTransit: true },
  storage: { volumePrefix: 'boxlite-volume', versioning: true },
  clickhouse: {
    mode: 'self-hosted',
    database: 'otel',
    writerUsername: 'otel_writer',
    readerUsername: 'otel_reader',
    instanceSize: 'medium',
    dataGb: 50,
  },
  runners: { size: 'large', rootDiskGb: 100 },
  alarms: {
    apiServerErrors: { threshold: 1, periods: 1 },
    proxyUnhealthyTargets: { threshold: 1, periods: 2 },
    runnersUnreachable: { threshold: 1, periods: 3 },
  },
}

const parse = (overrides: Record<string, unknown> = {}) =>
  parseDeployConfig('/repo/apps/infra/mdeploy.config.json', JSON.stringify({ ...valid, ...overrides }))

test('the defaults have to be complete, because nothing else supplies them', () => {
  // A stage says only what differs, so a gap there is the point. A gap in the
  // defaults is a value nothing supplies, and the resource would be created
  // from whatever the provider happened to default to.
  const { database, ...withoutDatabase } = valid
  assert.throws(
    () => parseDeployConfig('/repo/mdeploy.config.json', JSON.stringify(withoutDatabase)),
    /"database" must be an object/,
  )
  assert.throws(() => parse({ runners: { size: 'large' } }), /"runners" must set rootDiskGb/)
})

test('a stage is an override, not a copy', () => {
  const config = parse({ stages: { prod: { database: { protected: true, backupRetentionDays: 30 } } } })
  const prod = databaseFor(config, 'prod')
  assert.equal(prod.protected, true)
  assert.equal(prod.backupRetentionDays, 30)
  assert.equal(prod.name, 'boxlite', 'everything it did not mention comes from the defaults')
  assert.equal(databaseFor(config, 'dev').protected, false, 'and a stage it never named changes nothing')
})

test('alarms merge alarm by alarm, so retuning one keeps the others', () => {
  const config = parse({ stages: { prod: { alarms: { apiServerErrors: { threshold: 5, periods: 2 } } } } })
  const alarms = alarmsFor(config, 'prod')
  assert.deepEqual(alarms.apiServerErrors, { threshold: 5, periods: 2 })
  assert.deepEqual(alarms.runnersUnreachable, { threshold: 1, periods: 3 })
})

test('an unencrypted cache is refused rather than accepted and overridden', () => {
  // Accepting it and quietly turning encryption on would read as if the setting
  // worked. The cache carries sessions and box credentials across a network
  // every workload shares.
  assert.throws(() => parse({ cache: { ...valid.cache, encryptInTransit: false } }), /cannot be false/)
})

test('the two ClickHouse accounts must differ', () => {
  // One credential for both would let a compromised read path rewrite the
  // history it is reading, which is the whole reason there are two.
  assert.throws(
    () => parse({ clickhouse: { ...valid.clickhouse, readerUsername: 'otel_writer' } }),
    /writerUsername and readerUsername must differ/,
  )
})

test('a volume prefix that is not a bucket name on both clouds is refused', () => {
  // The prefix bounds what the API may delete, so a value two clouds could read
  // two ways is not one to accept.
  assert.throws(() => parse({ storage: { volumePrefix: 'BoxLite_Volume', versioning: true } }), /must match/)
  assert.throws(() => parse({ storage: { volumePrefix: 'x', versioning: true } }), /must match/)
})

test('a size no provider answers to is refused here rather than at the apply', () => {
  assert.throws(() => parse({ database: { ...valid.database, size: 'enormous' } }), /must be one of small, medium/)
  assert.throws(() => parse({ runners: { size: 'tiny', rootDiskGb: 100 } }), /must be one of small, medium, large/)
})

test('a key nothing reads is refused, so a typo is not silently inert', () => {
  assert.throws(() => parse({ database: { ...valid.database, retention: 7 } }), /does not take retention/)
  assert.throws(() => parse({ stages: { dev: { databse: {} } } }), /does not take databse/)
})

test('DeployConfigError is the single failure type callers can catch', () => {
  assert.throws(
    () => parse({ database: { ...valid.database, size: 'enormous' } }),
    (error) => error instanceof DeployConfigError,
  )
})

test('the repository’s own file parses, and every stage it names is deployable', () => {
  // Not a fixture: the real file, so a value added to one and not validated by
  // the other is caught here rather than at the first deploy that reads it.
  const path = new URL('../../mdeploy.config.json', import.meta.url)
  const real = parseDeployConfig(path.pathname, readFileSync(path, 'utf8'))
  assert.equal(real.database.name, 'boxlite')
  assert.equal(real.runners.size, 'large', 'a runner has to be a machine family that can nest')
  assert.equal(databaseFor(real, 'prod').protected, true, 'production refuses deletion')
  assert.equal(databaseFor(real, 'prod').highlyAvailable, true)
  assert.equal(cacheFor(real, 'prod').size, 'medium')
  assert.equal(clickHouseFor(real, 'gcp-dev').mode, 'disabled')
  assert.equal(storageFor(real, 'dev').versioning, true)
  assert.equal(runnersFor(real, 'dev').rootDiskGb, 100)
})

test('every stage mdeploy names is a stage mstage declares', () => {
  // The two files are read by different tools and neither validates the other,
  // so a stage overridden here but never declared there is an override that
  // silently applies to nothing.
  const deployPath = new URL('../../mdeploy.config.json', import.meta.url)
  const stagePath = new URL('../../mstage.config.json', import.meta.url)
  const deploy = parseDeployConfig(deployPath.pathname, readFileSync(deployPath, 'utf8'))
  const staged = JSON.parse(readFileSync(stagePath, 'utf8')) as { stages: Record<string, unknown> }
  for (const stage of Object.keys(deploy.stages)) {
    assert.ok(stage in staged.stages, `mdeploy.config.json overrides "${stage}", which mstage.config.json never names`)
  }
})
