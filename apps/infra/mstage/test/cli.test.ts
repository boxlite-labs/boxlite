import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { parseConfig } from '../src/config/load.ts'
import { UsageError, run, usage } from '../src/cli/run.ts'

// mstage is wired into apps/infra, not the repository root, so this is where
// `npm run mstage` resolves and where mstage.config.json is found.
const infraRoot = fileURLToPath(new URL('../..', import.meta.url))

const mstage = (...args: string[]) =>
  spawnSync('npm', ['run', '--silent', 'mstage', ...args], { cwd: infraRoot, encoding: 'utf8' })

test('an option left of the separator is reported by the real npm invocation', () => {
  // The swallow is npm's, so proving the guard requires going through npm.
  const result = mstage('aws', '--stage', 'dev', 'whoami')
  assert.equal(result.status, 1)
  assert.match(result.stderr, /--stage was consumed by npm/)
})

test('the same option right of the separator resolves the declared stage', () => {
  const result = mstage('aws', 'region', '--', '--stage', 'dev')
  assert.equal(result.status, 0, result.stderr)
  assert.match(result.stdout, /^ap-southeast-1$/m)
})

test('an undeclared stage is refused with the declared ones listed', () => {
  const result = mstage('aws', 'region', '--', '--stage', 'staging')
  assert.equal(result.status, 1)
  assert.match(result.stderr, /Stage "staging" \(from --stage\) is not declared/)
  assert.match(result.stderr, /Declared stages: dev/)
})

test('usage names every module and command, and where providers are declared', () => {
  const text = usage()
  for (const fragment of ['login', 'aws', 'whoami', 'region', 'exec', 'state', 'unlock', 'edit']) {
    assert.match(text, new RegExp(fragment))
  }
  assert.match(text, /declared per repository in mstage\.config\.json/)
})

test('an unknown module or command lists what exists', async () => {
  await assert.rejects(() => run({ argv: ['nope'], environment: {}, log() {} }), UsageError)
  await assert.rejects(() => run({ argv: ['nope'], environment: {}, log() {} }), /Known modules: login, aws/)
  await assert.rejects(
    () => run({ argv: ['aws', 'nope'], environment: {}, log() {} }),
    /Known commands: whoami, region, exec/,
  )
  // The dispatcher is the only thing that makes a command reachable, so each
  // module is asked what it registered rather than trusted to have registered it.
  await assert.rejects(
    () => run({ argv: ['state', 'nope'], environment: {}, log() {} }),
    /Unknown command "nope" for module state\. Known commands: unlock, edit/,
  )
})

test('a provider this repository does not declare is refused', async () => {
  await assert.rejects(
    () => run({ argv: ['login', 'okta'], environment: {}, log() {} }),
    /This repository does not use "okta"\. mstage\.config\.json declares: aws, github, auth0/,
  )
})

test('login providers come from mstage.config.json, not from mstage itself', () => {
  const config = parseConfig(
    '/repo/mstage.config.json',
    JSON.stringify({
      app: 'a',
      home: 'aws',
      login: { aws: {}, auth0: { required: false } },
      stages: { dev: { region: 'ap-southeast-1' } },
    }),
  )
  assert.deepEqual(config.login, { aws: { required: true }, auth0: { required: false } })
  assert.deepEqual(
    parseConfig('/repo/mstage.config.json', JSON.stringify({ app: 'a', home: 'aws', stages: { dev: {} } })).login,
    {},
  )
})

test('a command that takes no inner command rejects one', async () => {
  await assert.rejects(
    () => run({ argv: ['aws', 'region', '--stage', 'dev', '--', 'ls'], environment: {}, log() {} }),
    /takes no inner command/,
  )
})
