import assert from 'node:assert/strict'
import { createCipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { formatEnvironment } from '../src/cli/handlers/env.ts'
import { EnvError, readEnvironment, readStateBucket } from '../src/env/store.ts'

const KEY = randomBytes(32)

/**
 * Go writes `nonce || ciphertext || tag` (pkg/project/provider/provider.go:350).
 * Building the fixture that way here proves the reader handles that layout, but
 * not that Go produces it — only reading an object SST actually wrote does, and
 * that check runs against the live store rather than in this file.
 */
const sealed = (value: unknown, key: Buffer = KEY): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', key, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

const notFound = (name: string) => Object.assign(new Error(name), { name })

const clients = ({
  bootstrap = JSON.stringify({ version: 5, state: 'sst-state-abcdefghijkl' }) as string | Error,
  objects = {} as Record<string, Buffer | Error>,
  passphrases = {} as Record<string, Buffer | Error>,
}) => {
  const asked: string[] = []
  return {
    asked,
    ssm: {
      send: async (command: any) => {
        const name = command.input.Name
        asked.push(name)
        if (name === '/sst/bootstrap') {
          if (bootstrap instanceof Error) throw bootstrap
          return { Parameter: { Value: bootstrap } }
        }
        const value = passphrases[name]
        if (value === undefined) throw notFound('ParameterNotFound')
        if (value instanceof Error) throw value
        return { Parameter: { Value: value.toString('base64') } }
      },
    },
    s3: {
      send: async (command: any) => {
        const key = command.input.Key
        asked.push(`s3:${command.input.Bucket}/${key}`)
        const object = objects[key]
        if (object === undefined) throw notFound('NoSuchKey')
        if (object instanceof Error) throw object
        return { Body: { transformToByteArray: async () => object } }
      },
    },
  }
}

test('the state bucket comes from /sst/bootstrap, which nothing else records', async () => {
  const fake = clients({})
  assert.equal(await readStateBucket(fake), 'sst-state-abcdefghijkl')
  assert.deepEqual(fake.asked, ['/sst/bootstrap'])
})

test('an unbootstrapped region says so instead of failing on a bucket name', async () => {
  const fake = clients({ bootstrap: notFound('ParameterNotFound') as any })
  await assert.rejects(() => readStateBucket(fake), EnvError)
  await assert.rejects(() => readStateBucket(fake), /was never bootstrapped/)
})

test('a bootstrap parameter without a state bucket is refused', async () => {
  await assert.rejects(() => readStateBucket(clients({ bootstrap: JSON.stringify({ version: 5 }) })), /names no state/)
  await assert.rejects(() => readStateBucket(clients({ bootstrap: 'not json' })), /not valid JSON/)
})

test('a stage decrypts to its map, read from the SST key layout', async () => {
  const fake = clients({
    objects: { 'secret/boxlite/dev.json': sealed({ SMTP_USER: 'alice', PORT: '8080' }) },
    passphrases: { '/sst/passphrase/boxlite/dev': KEY },
  })
  const values = await readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' })
  assert.deepEqual(values, { SMTP_USER: 'alice', PORT: '8080' })
  assert.ok(fake.asked.includes('s3:sst-state-abcdefghijkl/secret/boxlite/dev.json'), fake.asked.join(', '))
  assert.ok(fake.asked.includes('/sst/passphrase/boxlite/dev'), fake.asked.join(', '))
})

test('a stage that was never written is empty, not an error', async () => {
  const empty = await readEnvironment({ clients: clients({}), app: 'boxlite', stage: 'never' })
  assert.deepEqual(empty, {})
})

test('a wrong passphrase is reported as such, not as corrupt JSON', async () => {
  const fake = clients({
    objects: { 'secret/boxlite/dev.json': sealed({ A: '1' }) },
    passphrases: { '/sst/passphrase/boxlite/dev': randomBytes(32) },
  })
  await assert.rejects(
    () => readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' }),
    /did not decrypt; the passphrase does not match/,
  )
})

test('a stored object with no passphrase names the parameter that is missing', async () => {
  const fake = clients({ objects: { 'secret/boxlite/dev.json': sealed({ A: '1' }) } })
  await assert.rejects(
    () => readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' }),
    /\/sst\/passphrase\/boxlite\/dev does not exist/,
  )
})

test('a passphrase that is not an AES key size is refused before decrypting', async () => {
  const fake = clients({
    objects: { 'secret/boxlite/dev.json': sealed({ A: '1' }) },
    passphrases: { '/sst/passphrase/boxlite/dev': randomBytes(20) },
  })
  await assert.rejects(() => readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' }), /20 bytes/)
})

test('a stage is read through the bucket its caller never has to name', async () => {
  const fake = clients({
    objects: { 'secret/boxlite/dev.json': sealed({ PORT: '8080' }) },
    passphrases: { '/sst/passphrase/boxlite/dev': KEY },
  })
  assert.deepEqual(await readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' }), { PORT: '8080' })
  // The bucket's name, the one object, and the key that opens it. No second
  // object: a stage's environment is one document.
  assert.deepEqual(fake.asked, [
    '/sst/bootstrap',
    's3:sst-state-abcdefghijkl/secret/boxlite/dev.json',
    '/sst/passphrase/boxlite/dev',
  ])
})

test('values are withheld unless they are asked for', () => {
  // `sst secret list` prints every value; one such command is enough to put a
  // private key into scrollback. Names answer "what is set" on their own.
  const named = formatEnvironment({
    app: 'boxlite',
    stage: 'dev',
    values: { SMTP_PASSWORD: 'hunter2' },
    withValues: false,
  })
  assert.deepEqual(named, ['# boxlite/dev', 'SMTP_PASSWORD'])
  assert.ok(!named.join('\n').includes('hunter2'), 'a value must not appear in the default output')
})

test('--values prints the format apps/api already parses', () => {
  // The header is included: sst-environment.store.ts:65 keys off "# <app>/<stage>"
  // to decide which section a line belongs to.
  assert.deepEqual(formatEnvironment({ app: 'boxlite', stage: 'dev', values: { PORT: '8080' }, withValues: true }), [
    '# boxlite/dev',
    'PORT=8080',
  ])
})

test('an empty store prints nothing, not a header with nothing under it', () => {
  assert.deepEqual(formatEnvironment({ app: 'a', stage: 'dev', values: {}, withValues: true }), [])
})
