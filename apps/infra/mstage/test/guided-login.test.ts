import assert from 'node:assert/strict'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import test from 'node:test'
import { run } from '../src/cli/run.ts'

const configRoot = (login: Record<string, { required?: boolean }>) => {
  const root = mkdtempSync(join(tmpdir(), 'mstage-guided-'))
  writeFileSync(
    join(root, 'mstage.config.json'),
    JSON.stringify({ app: 'a', home: 'aws', login, stages: { dev: { region: 'ap-southeast-1' } } }),
  )
  return root
}

/**
 * The AWS check reaches STS, so it is replaced wholesale here. What is under
 * test is the decision to interrupt, not any provider's own probe.
 */
const invoke = async ({
  login,
  interactive = true,
  answers = [] as boolean[],
  signInSucceeds = true,
  argv = ['login'],
}: {
  login: Record<string, { required?: boolean }>
  interactive?: boolean
  answers?: boolean[]
  signInSucceeds?: boolean
  argv?: string[]
}) => {
  const asked: string[] = []
  const signedIn: string[] = []
  const lines: string[] = []
  let ready = false

  const code = await run({
    argv,
    cwd: configRoot(login),
    environment: { AWS_REGION: 'ap-southeast-1' },
    log: (line: string) => lines.push(line),
    interactive,
    confirm: async (question: string) => {
      asked.push(question)
      return answers.shift() ?? false
    },
    signInWith: (provider: string) => {
      signedIn.push(provider)
      ready = signInSucceeds
      return signInSucceeds ? { ok: true } : { ok: false, detail: `${provider} sign-in failed` }
    },
    // Every provider answers from the same flag, so a sign-in visibly changes
    // what the re-check sees.
    checks: {
      aws: async () => statusOf('aws'),
      github: async () => statusOf('github'),
      auth0: async () => statusOf('auth0'),
    },
  } as any)

  function statusOf(provider: string) {
    return ready
      ? { provider, state: 'ready', detail: 'signed in', expiresAt: null }
      : { provider, state: 'not signed in', detail: 'no session', expiresAt: null }
  }

  return { code, asked, signedIn, text: lines.join('\n') }
}

test('a required provider that is not signed in prompts with the exact command', async () => {
  const { asked, signedIn, code } = await invoke({ login: { github: { required: true } }, answers: [true] })
  assert.equal(asked.length, 1)
  assert.match(asked[0]!, /Run `gh auth login` now\? \[y\/N\]/)
  assert.deepEqual(signedIn, ['github'])
  assert.equal(code, 0, 'the re-check sees the new session')
})

test('declining leaves it unsigned and the command fails', async () => {
  const { signedIn, code, text } = await invoke({ login: { github: { required: true } }, answers: [false] })
  assert.deepEqual(signedIn, [])
  assert.equal(code, 1)
  assert.match(text, /github {2}not signed in/)
})

test('an optional provider is never interrupted for', async () => {
  const { asked, signedIn, code } = await invoke({ login: { auth0: { required: false } } })
  assert.deepEqual(asked, [])
  assert.deepEqual(signedIn, [])
  assert.equal(code, 0, 'optional failures do not fail the command')
})

test('without a terminal nothing is asked, so CI reports instead of hanging', async () => {
  const { asked, signedIn, code } = await invoke({
    login: { github: { required: true } },
    interactive: false,
    answers: [true],
  })
  assert.deepEqual(asked, [])
  assert.deepEqual(signedIn, [])
  assert.equal(code, 1)
})

test('the session is re-checked, not inferred from the sign-in exit status', async () => {
  // `gh auth status --json` exits zero on a broken session; trusting a sign-in's
  // status the same way would report ready after a cancelled browser flow.
  const { code, text } = await invoke({
    login: { github: { required: true } },
    answers: [true],
    signInSucceeds: false,
  })
  assert.equal(code, 1)
  assert.match(text, /github sign-in failed/)
})

test('a provider already signed in is not offered a sign-in', async () => {
  const { asked, signedIn } = await invoke({
    login: { github: { required: true } },
    answers: [true],
    argv: ['login', 'github', '-f'],
  })
  // -f signs in first, so by the time the guided branch runs there is a session.
  assert.deepEqual(signedIn, ['github'])
  assert.deepEqual(asked, [], 'no prompt once the session is good')
})
