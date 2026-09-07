import assert from 'node:assert/strict'
import { createCipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { parseConfig } from '../src/config/load.ts'
import { ExportError, selectGroup } from '../src/env/select-group.ts'

const KEY = randomBytes(32)

const sealed = (value: unknown): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', KEY, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

const notFound = (name: string) => Object.assign(new Error(name), { name })

/** Objects keyed by version; 'current' is what S3 answers without a VersionId. */
const clients = (versions: Record<string, Record<string, string>>, key = 'secret/boxlite/dev.json') => {
  const reads: Record<string, unknown>[] = []
  return {
    reads,
    s3: {
      send: async (command: any) => {
        reads.push(command.input)
        if (command.input.Key !== key) throw notFound('NoSuchKey')
        const object = versions[(command.input.VersionId as string) ?? 'current']
        if (!object) throw notFound('NoSuchKey')
        return { Body: { transformToByteArray: async () => sealed(object) } }
      },
    },
    ssm: {
      send: async (command: any) =>
        command.input.Name === '/sst/bootstrap'
          ? { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
          : { Parameter: { Value: KEY.toString('base64') } },
    },
  }
}

const config = parseConfig(
  '/repo/mstage.config.json',
  JSON.stringify({
    app: 'boxlite',
    home: 'aws',
    env: { selectGroup: { server: ['OIDC_CLIENT_SECRET', 'ADMIN_API_KEY'] } },
    stages: { dev: { region: 'ap-southeast-1' } },
  }),
)

const STORED = { OIDC_CLIENT_SECRET: 'from-store', ADMIN_API_KEY: 'also', NOT_IN_THE_GROUP: 'stays home' }

test('a group answers with its own keys, and with nothing else the store holds', async () => {
  const fake = clients({ current: STORED })
  assert.deepEqual(await selectGroup({ group: 'server', stage: 'dev', clients: fake as any, config }), {
    OIDC_CLIENT_SECRET: 'from-store',
    ADMIN_API_KEY: 'also',
  })
})

test('the app comes from the config, so a consumer names only the stage', async () => {
  const fake = clients({ current: STORED })
  await selectGroup({ group: 'server', stage: 'dev', clients: fake as any, config })
  assert.equal(fake.reads.at(-1)?.Key, 'secret/boxlite/dev.json')
})

test('a key the group names but the store does not hold is refused, not omitted', async () => {
  // A process handed a silently short environment fails later, somewhere that
  // does not mention the missing key.
  const fake = clients({ current: { ADMIN_API_KEY: 'only this one' } })
  await assert.rejects(
    () => selectGroup({ group: 'server', stage: 'dev', clients: fake as any, config }),
    /the store is missing OIDC_CLIENT_SECRET, which env\.selectGroup\.server names/,
  )
})

test('a group the config does not declare names the ones it does', async () => {
  const fake = clients({ current: STORED })
  await assert.rejects(
    () => selectGroup({ group: 'nope', stage: 'dev', clients: fake as any, config }),
    /declares no "nope" under env\.selectGroup\. Declared: server/,
  )
})

test('a pinned version reads what the deploy saw, not what the store holds now', async () => {
  const fake = clients({
    current: { ...STORED, OIDC_CLIENT_SECRET: 'edited-since-the-deploy' },
    v3: STORED,
  })
  const values = await selectGroup({ group: 'server', stage: 'dev', versionId: 'v3', clients: fake as any, config })
  assert.equal(values.OIDC_CLIENT_SECRET, 'from-store')
  assert.equal(fake.reads.at(-1)?.VersionId, 'v3')
})

test('a stage or a region that was never given is refused before anything is read', async () => {
  const fake = clients({ current: STORED })
  await assert.rejects(
    () => selectGroup({ group: 'server', stage: '', clients: fake as any, config }),
    (error) => error instanceof ExportError && /needs the stage/.test(error.message),
  )
  await assert.rejects(() => selectGroup({ group: 'server', stage: 'dev', config }), /needs a region, or clients/)
  assert.equal(fake.reads.length, 0)
})
