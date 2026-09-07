import assert from 'node:assert/strict'
import test from 'node:test'
import { InvocationError, parseInvocation } from '../src/cli/argv.ts'

/**
 * The argv/env pairs below were measured against npm 11.18.0 by printing
 * `process.argv.slice(2)` and the `npm_config_*` environment from a package
 * script. They are what npm actually hands mstage, not a guess at its rules.
 */

test('an option to the right of the separator reaches mstage intact', () => {
  const invocation = parseInvocation(['aws', 'whoami', '--stage', 'dev'], {})
  assert.deepEqual(invocation, {
    module: 'aws',
    command: 'whoami',
    options: { stage: 'dev' },
    positionals: [],
    inner: null,
  })
})

test('the equals form is accepted alongside the spaced form', () => {
  const invocation = parseInvocation(['aws', 'whoami', '--stage=dev', '--region', 'ap-southeast-1'], {})
  assert.deepEqual(invocation.options, { stage: 'dev', region: 'ap-southeast-1' })
})

test('a module may stand alone without a command', () => {
  const invocation = parseInvocation(['login'], {})
  assert.deepEqual(invocation, { module: 'login', command: null, options: {}, positionals: [], inner: null })
})

test('a second separator starts the inner command and keeps its own flags', () => {
  const argv = ['aws', 'exec', '--stage', 'dev', '--', 'npx', 'sst', 'deploy', '--stage', 'prod']
  const invocation = parseInvocation(argv, {})
  assert.deepEqual(invocation.options, { stage: 'dev' })
  assert.deepEqual(invocation.inner, ['npx', 'sst', 'deploy', '--stage', 'prod'])
})

test('npm eats the separator before an inner command, so a bare positional starts it', () => {
  const invocation = parseInvocation(['aws', 'exec', 'npx', 'sst', 'deploy', '--stage', 'dev'], {})
  assert.deepEqual(invocation.inner, ['npx', 'sst', 'deploy', '--stage', 'dev'])
  // The same tokens read the other way, for a command that takes an argument
  // rather than a command. Only the caller can tell which reading applies.
  assert.deepEqual(invocation.positionals, ['npx', 'sst', 'deploy'])
  assert.deepEqual(invocation.options, { stage: 'dev' })
})

test('an argument may be followed by options, as `env set KEY=V --stage dev` is', () => {
  const invocation = parseInvocation(['env', 'set', 'SMTP_USER=alice', '--stage', 'dev'], {})
  assert.deepEqual(invocation.positionals, ['SMTP_USER=alice'])
  assert.deepEqual(invocation.options, { stage: 'dev' })
})

test('a flag option needs no value', () => {
  assert.equal(parseInvocation(['secret', 'remove', '--confirm'], {}).options.confirm, true)
  assert.equal(parseInvocation(['secret', 'remove', '--confirm=false'], {}).options.confirm, false)
})

test('the spaced form written before the separator is reported, not acted on', () => {
  // npm run mstage aws --stage dev whoami
  assert.throws(
    () => parseInvocation(['aws', 'dev', 'whoami'], { npm_config_stage: 'true' }),
    (error) => {
      assert.ok(error instanceof InvocationError)
      assert.match(error.message, /--stage was consumed by npm/)
      return true
    },
  )
})

test('the equals form written before the separator is reported, not silently dropped', () => {
  // npm run mstage aws --stage=dev whoami
  assert.throws(() => parseInvocation(['aws', 'whoami'], { npm_config_stage: 'dev' }), /--stage was consumed by npm/)
})

test('a dashed option name maps to its npm_config underscore form', () => {
  // npm run mstage aws --role-arn=arn:aws:iam::1:role/x whoami
  assert.throws(
    () => parseInvocation(['aws', 'whoami'], { npm_config_role_arn: 'arn:aws:iam::1:role/x' }),
    /--role-arn was consumed by npm/,
  )
})

test('an option supplied correctly is not mistaken for a swallowed one', () => {
  const invocation = parseInvocation(['aws', 'whoami', '--stage', 'dev'], { npm_config_stage: 'from-npmrc' })
  assert.equal(invocation.options.stage, 'dev')
})

test('a garbled invocation still reports the swallow rather than the parse failure', () => {
  assert.throws(() => parseInvocation(['dev', 'aws', '--nonsense'], { npm_config_stage: 'true' }), /consumed by npm/)
})

test('unknown, repeated, valueless and short options are rejected', () => {
  assert.throws(() => parseInvocation(['aws', 'whoami', '--nope', 'x'], {}), /Unknown option --nope/)
  assert.throws(() => parseInvocation(['aws', 'whoami', '--stage', 'a', '--stage', 'b'], {}), /given more than once/)
  assert.throws(() => parseInvocation(['aws', 'whoami', '--stage'], {}), /--stage requires a value/)
  assert.throws(() => parseInvocation(['aws', 'whoami', '--stage', '--region', 'x'], {}), /--stage requires a value/)
  assert.throws(() => parseInvocation(['aws', 'whoami', '-s', 'dev'], {}), /only short options are: -f \(--force\)/)
  // Both unknown-option errors name the module, so the suggestion is runnable.
  assert.throws(() => parseInvocation(['env', 'list', '-h'], {}), /For usage: npm run mstage env -- --help/)
  assert.throws(() => parseInvocation(['aws', 'whoami', '--nope', 'x'], {}), /For usage: npm run mstage aws -- --help/)
})

test('a missing module and an option in the module slot are rejected', () => {
  assert.throws(() => parseInvocation([], {}), /usage: npm run mstage/)
  assert.throws(() => parseInvocation(['--stage', 'dev'], {}), /Expected a module name/)
})

test('a module with no command still takes options, as `mstage deploy -- --stage dev` does', () => {
  const invocation = parseInvocation(['deploy', '--stage', 'dev'], {})
  assert.deepEqual(invocation, {
    module: 'deploy',
    command: null,
    options: { stage: 'dev' },
    positionals: [],
    inner: null,
  })
})

test('-f is the one short option, and it may stand in the command slot', () => {
  assert.deepEqual(parseInvocation(['login', '-f'], {}), {
    module: 'login',
    command: null,
    options: { force: true },
    positionals: [],
    inner: null,
  })
  assert.equal(parseInvocation(['login', 'aws', '-f'], {}).options.force, true)
  assert.equal(parseInvocation(['login', 'aws', '--force'], {}).options.force, true)
  assert.throws(() => parseInvocation(['login', '-f', '-f'], {}), /--force was given more than once/)
  assert.throws(() => parseInvocation(['deploy', '-s', 'dev'], {}), /only short options are: -f \(--force\)/)
})

test("-f left of the separator is npm's own --force, and is reported", () => {
  // Measured: `npm run mstage login -f` sets npm_config_force=true and drops it from argv.
  assert.throws(() => parseInvocation(['login'], { npm_config_force: 'true' }), /--force was consumed by npm/)
  assert.equal(parseInvocation(['login', '-f'], {}).options.force, true)
})
