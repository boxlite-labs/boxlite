import assert from 'node:assert/strict'
import test from 'node:test'
import { ConfigError, homeFor, parseConfig } from '../src/config/load.ts'

const valid = {
  app: 'boxlite',
  home: 'aws',
  stages: {
    dev: { region: 'ap-southeast-1' },
    prod: { region: 'ap-southeast-1', protect: true },
  },
}

const parse = (overrides: any) => parseConfig('/repo/mstage.config.json', JSON.stringify({ ...valid, ...overrides }))

test('a complete config keeps every declared stage field', () => {
  const config = parse({})
  assert.equal(config.app, 'boxlite')
  assert.equal(config.root, '/repo')
  assert.deepEqual(config.stages.dev, {
    region: 'ap-southeast-1',
    home: null,
    project: null,
    roleArn: null,
    protect: false,
  })
  assert.equal(config.stages.prod.protect, true)
})

test('a stage takes the repository home unless it declares its own', () => {
  // BoxLite's own shape: the AWS stages are in service while a GCP one is being
  // brought up beside them, and both have to be deployable from one checkout.
  const config = parse({
    stages: {
      dev: { region: 'ap-southeast-1' },
      'gcp-dev': { region: 'asia-southeast1', home: 'gcp', project: 'boxlite-gcp-dev' },
    },
  })
  assert.equal(config.stages.dev!.home, null, 'silence is not a value; it is a deferral to the repository')
  assert.equal(homeFor(config, 'dev'), 'aws')
  assert.equal(homeFor(config, 'gcp-dev'), 'gcp')
})

test('a stage that lives in gcp must name its project, and a foreign home is refused', () => {
  // The clients cannot be built without a project, so the config file — the only
  // thing that could supply one — is where the gap is worth naming.
  assert.throws(
    () => parse({ stages: { dev: { region: 'asia-southeast1', home: 'gcp' } } }),
    /lives in gcp and must declare a project/,
  )
  assert.throws(() => parse({ stages: { dev: { region: 'x', home: 'azure' } } }), /home must be "aws" or "gcp"/)
})

test('homeFor refuses a stage the config never declared rather than guessing the default', () => {
  assert.throws(() => homeFor(parse({}), 'dve'), /declares no stage "dve"\. Declared: dev, prod/)
})

test('a GCP stage declares the project it lives in; an AWS stage declares no tenant at all', () => {
  // The AWS account is read back from the credentials by whoever has to name it
  // in an ARN, so there is nothing here to keep in step with it. GCP's clients
  // cannot be built without a project, so that one is declared.
  const config = parse({ home: 'gcp', stages: { dev: { region: 'asia-southeast1', project: 'boxlite-dev' } } })
  assert.equal(config.stages.dev!.project, 'boxlite-dev')
  assert.equal(parse({}).stages.dev!.project, null)
})

test("how a stage deploys is not the shared config's business", () => {
  // private/deploy owns it, so a stray "deploy" key must not become a silent contract.
  const config = parse({ deploy: { command: ['npm', 'run', 'deploy'] } })
  assert.equal((config as Record<string, unknown>).deploy, undefined)
})

test('malformed JSON names the file', () => {
  assert.throws(() => parseConfig('/repo/mstage.config.json', '{'), /\/repo\/mstage\.config\.json is not valid JSON/)
})

test('a missing app, a foreign home and an empty stage map are rejected', () => {
  assert.throws(() => parse({ app: '' }), /"app" must be a non-empty string/)
  assert.throws(() => parse({ home: 'cloudflare' }), /"home" must be "aws"/)
  assert.throws(() => parse({ stages: {} }), /must declare at least one stage/)
})

test('a stage name SST would reject is refused before it can reach the bucket', () => {
  assert.throws(() => parse({ stages: { 'pr/42': { region: 'ap-southeast-1' } } }), /may only contain letters/)
  assert.throws(() => parse({ stages: { staging_2: { region: 'ap-southeast-1' } } }), /may only contain letters/)
})

test('a malformed project and protect flag are rejected', () => {
  assert.throws(() => parse({ stages: { dev: { project: '  ' } } }), /project must be a non-empty string/)
  assert.throws(() => parse({ stages: { dev: { protect: 'yes' } } }), /protect must be true or false/)
})

test('ConfigError is the single failure type callers can catch', () => {
  assert.throws(
    () => parse({ app: '' }),
    (error) => error instanceof ConfigError,
  )
})
