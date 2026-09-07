import assert from 'node:assert/strict'
import { createCipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { list } from '../src/cli/handlers/env.ts'

const KEY = randomBytes(32)

const sealed = (value: unknown): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', KEY, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

const SECRET = 'cf-token-that-must-not-be-printed'

/** `comma` swaps the token for a value that would have been split. */
const shown = async (options: Record<string, string | boolean>): Promise<string[]> => {
  const { comma, ...passed } = options
  const token = comma === true ? 'A,B,C,D' : SECRET
  const lines: string[] = []
  await list({
    config: { path: '/repo/mstage.config.json', envSelectGroup: { deploy: ['DOMAIN', 'TOKEN'] } } as any,
    scope: { app: 'a', stage: 'dev' } as any,
    options: passed,
    log: (line: string) => lines.push(line),
    backend: {
      s3: {
        send: async () => ({
          Body: { transformToByteArray: async () => sealed({ DOMAIN: 'example.com', TOKEN: token }) },
        }),
      },
      ssm: {
        send: async (command: any) =>
          command.input.Name === '/sst/bootstrap'
            ? { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
            : { Parameter: { Value: KEY.toString('base64') } },
      },
    } as any,
  })
  return lines
}

test('--select-group narrows to the group without printing what is in it', async () => {
  const lines = await shown({ 'select-group': 'deploy' })
  assert.deepEqual(lines.slice(0, 2), ['DOMAIN', 'TOKEN'])
  assert.equal(
    lines.join('\n').includes(SECRET),
    false,
    'naming a group is not the same as asking to see a credential in it',
  )
  assert.match(lines.join('\n'), /add --values to print them/)
})

test('--select-group --values is the explicit ask, and prints KEY=VALUE', async () => {
  assert.deepEqual(await shown({ 'select-group': 'deploy', values: true }), ['DOMAIN=example.com', `TOKEN=${SECRET}`])
})

test('--select-group --json still exports the group for a file another tool reads', async () => {
  const [payload] = await shown({ 'select-group': 'deploy', json: true })
  assert.deepEqual(JSON.parse(payload as string), { DOMAIN: 'example.com', TOKEN: SECRET })
})

test('a value with commas prints as the one string it is', async () => {
  // `env set -- KEY=A,B,C,D` stores four characters and three commas. Printing
  // that as a list would invent a structure the store never held, and would cut
  // any value that merely contains a comma — a JSON document, a subject line —
  // into pieces.
  const [payload] = await shown({ json: true, comma: true })
  assert.deepEqual(JSON.parse(payload as string), { DOMAIN: 'example.com', TOKEN: 'A,B,C,D' })
})

test('without --select-group the whole store is still names-only by default', async () => {
  const lines = await shown({})
  assert.equal(lines.join('\n').includes(SECRET), false)
  assert.deepEqual(await shown({ values: true }), [
    '# a/dev',
    'DOMAIN=example.com',
    `TOKEN=${SECRET}`,
  ])
})
