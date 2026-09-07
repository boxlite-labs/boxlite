import assert from 'node:assert/strict'
import { createCipheriv, createDecipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { EnvError, SECRET_NAME, readEnvironment, setValues } from '../src/env/store.ts'

const KEY = randomBytes(32)

const sealed = (value: unknown): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', KEY, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

/** Opens what the writer sealed, the way the Go reader would. */
const opened = (payload: Buffer): Record<string, string> => {
  const nonce = payload.subarray(0, 12)
  const tag = payload.subarray(payload.length - 16)
  const decipher = createDecipheriv('aes-256-gcm', KEY, nonce)
  decipher.setAuthTag(tag)
  const body = payload.subarray(12, payload.length - 16)
  return JSON.parse(Buffer.concat([decipher.update(body), decipher.final()]).toString('utf8'))
}

const notFound = (name: string) => Object.assign(new Error(name), { name })

const clients = ({ stored, passphrase = KEY }: { stored?: Record<string, string>; passphrase?: Buffer } = {}) => {
  const puts: any[] = []
  const store: Record<string, Buffer> = stored ? { 'secret/boxlite/dev.json': sealed(stored) } : {}
  return {
    puts,
    store,
    ssm: {
      send: async (command: any) => {
        const name = command.input.Name
        if (name === '/sst/bootstrap') return { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
        if (name === '/sst/passphrase/boxlite/dev') return { Parameter: { Value: passphrase.toString('base64') } }
        throw notFound('ParameterNotFound')
      },
    },
    s3: {
      send: async (command: any) => {
        if (command.input.Body) {
          puts.push(command.input)
          store[command.input.Key] = command.input.Body
          return {}
        }
        const object = store[command.input.Key]
        if (!object) throw notFound('NoSuchKey')
        return { Body: { transformToByteArray: async () => object } }
      },
    },
  }
}

test('a new key is added without disturbing the ones already there', async () => {
  const fake = clients({ stored: { PORT: '8080', SMTP_USER: 'alice' } })
  const result = await setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [['NEW_KEY', 'v']] })
  assert.deepEqual(result.outcomes, [{ name: 'NEW_KEY', existed: false, unchanged: false }])
  assert.deepEqual(opened(fake.puts[0].Body), { PORT: '8080', SMTP_USER: 'alice', NEW_KEY: 'v' })
})

test('what is written can be read back through the reader that SST shares', async () => {
  const fake = clients({ stored: { PORT: '8080' } })
  await setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [['PORT', '9090']] })
  const reread = await readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' })
  assert.deepEqual(reread, { PORT: '9090' }, 'the object must stay in the layout the reader expects')
})

test('replacing reports that it replaced, and setting the same value writes nothing', async () => {
  const fake = clients({ stored: { PORT: '8080' } })
  assert.equal(
    (await setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [['PORT', '9090']] })).outcomes[0]!
      .existed,
    true,
  )
  assert.equal(fake.puts.length, 1)

  const same = clients({ stored: { PORT: '8080' } })
  const result = await setValues({ clients: same, app: 'boxlite', stage: 'dev', entries: [['PORT', '8080']] })
  assert.equal(result.outcomes[0]!.unchanged, true)
  assert.equal(same.puts.length, 0, 'an unchanged value must not rewrite the object')
})

test('the object is sealed with a fresh nonce each time', async () => {
  const fake = clients({ stored: { A: '1' } })
  await setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [['A', '2']] })
  await setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [['A', '3']] })
  const [first, second] = fake.puts
  assert.notDeepEqual(first.Body.subarray(0, 12), second.Body.subarray(0, 12), 'a repeated nonce breaks GCM')
})

test('a name SST would reject never reaches the bucket', async () => {
  // cmd/sst/secret.go:363 — a name SST cannot set is one SST cannot read back.
  const fake = clients({ stored: { A: '1' } })
  for (const name of ['lowercase', '9LEADING', 'HAS-DASH', '']) {
    await assert.rejects(
      () => setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [[name, 'v']] }),
      EnvError,
      `"${name}" should have been refused`,
    )
  }
  assert.equal(fake.puts.length, 0)
  assert.ok(SECRET_NAME.test('BACKOFFICE_STAGE_CONFIG'))
})

test('a store with no passphrase is not written to, and no passphrase is invented', async () => {
  // SST creates one with Overwrite=false and "DO NOT DELETE STATE WILL BECOME
  // UNRECOVERABLE"; making that key as a side effect of a set is not this
  // command's decision.
  const fake = clients({ stored: { A: '1' } })
  const noPassphrase = {
    ...fake,
    ssm: {
      send: async (command: any) => {
        if (command.input.Name === '/sst/bootstrap') {
          return { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
        }
        throw notFound('ParameterNotFound')
      },
    },
  }
  await assert.rejects(
    () => setValues({ clients: noPassphrase, app: 'boxlite', stage: 'dev', entries: [['A', 'v']] }),
    /\/sst\/passphrase\/boxlite\/dev does not exist/,
  )
  assert.equal(fake.puts.length, 0)
})

const PEM = [
  '-----BEGIN RSA PRIVATE KEY-----',
  'MIIEpAIBAAKCAQEA',
  'r1rNbFS0eJYbngek',
  '-----END RSA PRIVATE KEY-----',
].join('\n')

test('a multi-line value survives the write and the read', async () => {
  // The store already holds a PEM this shape; JSON carries the newlines and the
  // ciphertext is opaque to them.
  const fake = clients({ stored: { A: '1' } })
  await setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [['PRIVATE_KEY', PEM]] })
  const reread = await readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' })
  assert.equal(reread.PRIVATE_KEY, PEM)
  assert.equal(reread.PRIVATE_KEY!.split('\n').length, 4)
})

test('an "=" inside the value is part of the value, not a second assignment', async () => {
  const fake = clients({ stored: {} })
  await setValues({ clients: fake, app: 'boxlite', stage: 'dev', entries: [['B64', 'aGVsbG8=']] })
  const reread = await readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' })
  assert.equal(reread.B64, 'aGVsbG8=')
})
