import assert from 'node:assert/strict'
import { createCipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { list } from '../src/cli/handlers/env.ts'
import { parseInvocation, VALUE_OPTIONS } from '../src/cli/argv.ts'

const KEY = randomBytes(32)

const sealed = (value: unknown): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', KEY, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

const notFound = (name: string) => Object.assign(new Error(name), { name })

const shown = async (options: Record<string, string | boolean>, versions: Record<string, unknown>) => {
  const lines: string[] = []
  const asked: Record<string, unknown>[] = []
  await list({
    config: { path: '/repo/mstage.config.json', envSelectGroup: { deploy: ['A'] } } as any,
    scope: { app: 'a', stage: 'dev' } as any,
    options,
    log: (line: string) => lines.push(line),
    backend: {
      s3: {
        send: async (command: any) => {
          asked.push(command.input)
          const object = versions[command.input.VersionId ?? 'current']
          if (object === undefined) throw notFound('NoSuchKey')
          return { Body: { transformToByteArray: async () => sealed(object) } }
        },
      },
      ssm: {
        send: async (command: any) =>
          command.input.Name === '/sst/bootstrap'
            ? { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
            : { Parameter: { Value: KEY.toString('base64') } },
      },
    } as any,
  })
  return { lines, asked }
}

const STORE = { current: { A: 'now' }, 'hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq': { A: 'then' } }

test('--version reads that object version instead of the current one', async () => {
  const result = await shown({ version: 'hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq', values: true }, STORE)
  assert.ok(result.lines.includes('A=then'), 'the pinned value, not the current one')
  assert.equal(result.asked.at(-1)?.VersionId, 'hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq')
})

test('the heading names the version, so two listings are told apart', async () => {
  const pinned = await shown({ version: 'hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq' }, STORE)
  assert.equal(pinned.lines[0], '# a/dev @ hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq')
  const current = await shown({}, STORE)
  assert.equal(current.lines[0], '# a/dev', 'and says nothing extra when reading current')
})

test('a version that is gone is reported, not quietly answered from current', async () => {
  await assert.rejects(() => shown({ version: 'expired' }, STORE), /has no version expired/)
})

test('--version pairs with --select-group, so a group can be read as it was deployed', async () => {
  const result = await shown({ version: 'hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq', 'select-group': 'deploy', values: true }, STORE)
  assert.deepEqual(result.lines, ['A=then'])
})

test('the parser takes --version as a value, not a flag', () => {
  assert.ok(VALUE_OPTIONS.includes('version'))
  // npm eats the first `--`, so this is what bin/mstage.ts actually receives.
  const parsed = parseInvocation(['env', 'list', '--stage=dev', '--version=hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq'], {})
  // A version id carries "." and "-"; splitting on the first "=" keeps it whole.
  assert.equal(parsed.options.version, 'hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq')
})
