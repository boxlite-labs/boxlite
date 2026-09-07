import assert from 'node:assert/strict'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { SIGN_OUT_COMMANDS, signOut } from '../src/auth/sessions.ts'
import { run } from '../src/cli/run.ts'

const configRoot = (login: Record<string, { required?: boolean }>) => {
  const root = mkdtempSync(join(tmpdir(), 'mstage-logout-'))
  writeFileSync(
    join(root, 'mstage.config.json'),
    JSON.stringify({ app: 'a', home: 'aws', login, stages: { dev: { region: 'ap-southeast-1' } } }),
  )
  return root
}

test('each provider knows the command that ends its session', () => {
  assert.deepEqual(SIGN_OUT_COMMANDS, {
    aws: ['aws', 'logout'],
    gcp: ['gcloud', 'auth', 'application-default', 'revoke'],
    github: ['gh', 'auth', 'logout'],
    auth0: ['auth0', 'logout'],
  })
})

test('a sign-out inherits the terminal, because gh asks which account', () => {
  const calls: any[] = []
  const runCommand = (command: string, args: string[], options: any) => {
    calls.push({ command, args, options })
    return { status: 0 }
  }
  assert.deepEqual(signOut('github', runCommand as any), { ok: true })
  assert.deepEqual(calls, [{ command: 'gh', args: ['auth', 'logout'], options: { stdio: 'inherit' } }])
})

test('a failed sign-out names the command that failed', () => {
  assert.deepEqual(signOut('auth0', (() => ({ status: 1 })) as any), {
    ok: false,
    detail: 'auth0 logout exited with 1',
  })
})

const invoke = async ({
  argv,
  login = { github: { required: true } },
  ready = true,
}: {
  argv: string[]
  login?: Record<string, { required?: boolean }>
  ready?: boolean
}) => {
  const signedOut: string[] = []
  const signedIn: string[] = []
  const asked: string[] = []
  const lines: string[] = []
  const code = await run({
    argv,
    cwd: configRoot(login),
    environment: { AWS_REGION: 'ap-southeast-1' },
    log: (line: string) => lines.push(line),
    interactive: true,
    confirm: async (question: string) => {
      asked.push(question)
      return true
    },
    signInWith: (provider: string) => {
      signedIn.push(provider)
      return { ok: true }
    },
    signOutWith: (provider: string) => {
      signedOut.push(provider)
      return { ok: true }
    },
    checks: {
      aws: async () => status('aws'),
      github: async () => status('github'),
      auth0: async () => status('auth0'),
    },
  } as any)
  function status(provider: string) {
    return ready
      ? { provider, state: 'ready', detail: 'signed in', expiresAt: null }
      : { provider, state: 'not signed in', detail: 'no session', expiresAt: null }
  }
  return { code, signedOut, signedIn, asked, text: lines.join('\n') }
}

test('--logout ends only the named provider and announces the command', async () => {
  const { signedOut, text } = await invoke({ argv: ['login', 'github', '--logout'] })
  assert.deepEqual(signedOut, ['github'])
  assert.match(text, /github {2}signing out: gh auth logout/)
})

test('--logout without a provider ends every one this repository declares', async () => {
  const { signedOut } = await invoke({
    argv: ['login', '--logout'],
    login: { aws: { required: true }, auth0: { required: false } },
  })
  assert.deepEqual(signedOut, ['aws', 'auth0'])
})

test('after signing out nobody is offered a sign-in', async () => {
  // The guided prompt exists for someone who wants a session, which is the
  // opposite of what --logout just asked for.
  const { asked, signedIn, code } = await invoke({ argv: ['login', 'github', '--logout'], ready: false })
  assert.deepEqual(asked, [])
  assert.deepEqual(signedIn, [])
  assert.equal(code, 1, 'the report still says the required session is gone')
})

test('--logout and --force together are refused rather than ordered', async () => {
  await assert.rejects(
    () => invoke({ argv: ['login', 'github', '--logout', '--force'] }),
    /--logout and --force ask for opposite things/,
  )
})
