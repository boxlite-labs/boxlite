import assert from 'node:assert/strict'
import test from 'node:test'
import { FLAG_OPTIONS, OPTION_NAMES, VALUE_OPTIONS } from '../src/cli/argv.ts'
import { OPTION_HELP, moduleUsage } from '../src/cli/help.ts'
import { run } from '../src/cli/run.ts'

const helpFor = async (module: string) => {
  const lines: string[] = []
  const code = await run({ argv: [module, '--help'], environment: {}, log: (line: string) => lines.push(line) })
  return { code, text: lines.join('\n') }
}

test('every option mstage parses has a description, so help cannot go stale', () => {
  for (const option of OPTION_NAMES) {
    assert.ok(OPTION_HELP[option], `--${option} is parsed but undocumented`)
  }
})

test('a value option is shown in the = form, which survives every position', async () => {
  const { text } = await helpFor('aws')
  assert.match(text, /--stage=<stage>/)
  for (const option of VALUE_OPTIONS) {
    assert.ok(!text.includes(`--${option} <`), `--${option} must not be shown in the spaced form`)
  }
})

test('a flag is shown without a value, and its short alias is named', async () => {
  const { text } = await helpFor('login')
  assert.match(text, /-f, --force/)
  assert.match(text, /^\s+--logout\s{2,}/m)
  assert.ok(FLAG_OPTIONS.includes('force'))
})

test('required options are marked, and an inner command counts as one', async () => {
  const { text } = await helpFor('aws')
  assert.match(text, /--stage=<stage>\s+required ·/)
  assert.match(text, /-- <command> \[args…\]\s+required ·/)
})

test('options shared by every command are listed once, not per command', async () => {
  const { text } = await helpFor('aws')
  assert.equal(text.split('--role-arn').length - 1, 1, 'a shared option must appear once')
  assert.match(text, /every command also takes/)
})

test('login lists every provider mstage can check, not only the enabled ones', async () => {
  // mstage.config.json selects which a repository requires; it does not define the
  // set, so a provider this repository has not enabled is still documented.
  const { text } = await helpFor('login')
  for (const provider of ['aws', 'github', 'auth0']) {
    assert.match(text, new RegExp(`^ {2}${provider}\\s{2,}\\S`, 'm'), `${provider} is missing from login help`)
  }
  assert.match(text, /omit the command to act on every provider mstage\.config\.json enables/)
})

test('a command list is described after it, where the note can qualify it', async () => {
  const { text } = await helpFor('login')
  assert.ok(text.indexOf('  aws ') < text.indexOf('omit the command'), 'the note explains the list, so it follows it')
})

test("the example is the module's own, not a generic one that would be wrong", async () => {
  // login takes no --stage; an example claiming otherwise is worse than none.
  const login = await helpFor('login')
  assert.ok(!login.text.includes('--stage'), login.text)
  assert.match(
    (await helpFor('env')).text,
    /example: npm run mstage env list -- --stage=dev --select-group=deploy --json > \.deploy\.env\.json/,
  )
})

test('every usage line names the separator npm would otherwise eat', async () => {
  for (const module of ['login', 'aws', 'env', 'state']) {
    const { text, code } = await helpFor(module)
    assert.equal(code, 0)
    assert.match(text, /every option must sit to the right of the "--"/)
    assert.match(text, new RegExp(`usage: npm run mstage ${module} <command> -- \\[options\\]`))
  }
})

test('help for an unknown module lists the real ones', async () => {
  await assert.rejects(() => helpFor('nope'), /Known modules: login, aws, env, state$/)
})

test('the rendered width adapts to the longest option, so nothing collides', () => {
  const text = moduleUsage('x', {
    summary: 's',
    scope: 'stage',
    example: 'e',
    commands: { go: { run: async () => 0, summary: 'go', requires: ['role-session-name', 'stage'] } },
  })
  for (const line of text.split('\n').filter((line) => line.includes('required ·'))) {
    assert.match(line, /\S {2,}required ·/, `columns collided: ${line}`)
  }
})
