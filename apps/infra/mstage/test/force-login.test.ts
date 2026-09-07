import assert from 'node:assert/strict'
import test from 'node:test'
import { SIGN_IN_COMMANDS, signIn } from '../src/auth/sessions.ts'
import { run } from '../src/cli/run.ts'
import { loadConfig } from '../src/config/load.ts'

test('each provider knows the command that signs it in', () => {
  assert.deepEqual(SIGN_IN_COMMANDS, {
    aws: ['aws', 'login'],
    gcp: ['gcloud', 'auth', 'application-default', 'login'],
    github: ['gh', 'auth', 'login'],
    auth0: ['auth0', 'login'],
  })
})

test('a sign-in inherits the terminal, because it prompts or opens a browser', () => {
  const calls: any[] = []
  const runCommand = (command: string, args: string[], options: any) => {
    calls.push({ command, args, options })
    return { status: 0 }
  }
  assert.deepEqual(signIn('github', runCommand), { ok: true })
  assert.deepEqual(calls, [{ command: 'gh', args: ['auth', 'login'], options: { stdio: 'inherit' } }])
})

test('a failed or missing sign-in reports rather than throws', () => {
  assert.deepEqual(
    signIn('auth0', () => ({ status: 1 })),
    {
      ok: false,
      detail: 'auth0 login exited with 1',
    },
  )
  // The AWS formula is not named after its binary, so the hint is read from the
  // provider table rather than derived from the command.
  assert.deepEqual(
    signIn('aws', () => ({ error: Object.assign(new Error('x'), { code: 'ENOENT' }) })),
    {
      ok: false,
      detail: 'aws is not installed. Install it with: brew install awscli',
    },
  )
})

/**
 * The force path is exercised through `run`, the only place that decides whether
 * a sign-in happens. The real sign-ins are interactive — they prompt or open a
 * browser — so the seam is injected rather than letting a test fire them.
 */
const captureRun = async (argv: string[]) => {
  const lines: string[] = []
  const signedIn: string[] = []
  await run({
    argv,
    environment: {},
    log: (line: string) => lines.push(line),
    signInWith: (provider: string) => {
      signedIn.push(provider)
      return { ok: true }
    },
  })
  return { signedIn, text: lines.join('\n') }
}

test('without --force nothing is signed in, only reported', async () => {
  const { signedIn, text } = await captureRun(['login', 'github'])
  assert.deepEqual(signedIn, [])
  assert.ok(!text.includes('signing in'), text)
})

test('--force signs in only the named provider, and announces the command', async () => {
  const { signedIn, text } = await captureRun(['login', 'github', '--force'])
  assert.deepEqual(signedIn, ['github'])
  assert.match(text, /github {2}signing in: gh auth login/)
})

test('-f forces every provider this repository enables, not every one mstage knows', async () => {
  // The two lists are different: mstage knows four clouds and CLIs, and
  // mstage.config.json says which of them this console actually needs. Forcing
  // a sign-in nobody declared would open a browser for a cloud unrelated to the
  // work.
  const { signedIn } = await captureRun(['login', '-f'])
  const enabled = Object.keys(loadConfig({ cwd: new URL('../..', import.meta.url).pathname }).login)
  assert.deepEqual(signedIn, enabled)
  assert.ok(Object.keys(SIGN_IN_COMMANDS).length > enabled.length, 'mstage knows more than this repository enables')
})

test('the session is read after the sign-in, not before', async () => {
  const order: string[] = []
  const lines: string[] = []
  await run({
    argv: ['login', 'github', '-f'],
    environment: {},
    log: (line: string) => {
      lines.push(line)
      if (line.includes('github')) order.push(line.includes('signing in') ? 'sign-in' : 'report')
    },
    signInWith: () => ({ ok: true }),
  })
  assert.equal(order[0], 'sign-in', lines.join('\n'))
  assert.equal(order[1], 'report', lines.join('\n'))
})

test('a failed sign-in is reported and the check still runs', async () => {
  const lines: string[] = []
  await run({
    argv: ['login', 'auth0', '-f'],
    environment: {},
    log: (line: string) => lines.push(line),
    signInWith: () => ({ ok: false, detail: 'auth0 login exited with 1' }),
  })
  const text = lines.join('\n')
  assert.match(text, /auth0 login exited with 1/)
  assert.match(text, /auth0 {3}(ready|not signed in)/)
})
