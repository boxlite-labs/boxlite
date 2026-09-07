import assert from 'node:assert/strict'
import { createCipheriv, createDecipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { del } from '../src/cli/handlers/env.ts'
import { deleteValues, readEnvironment } from '../src/env/store.ts'

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

const clients = ({ stored }: { stored?: Record<string, string> } = {}) => {
  const puts: any[] = []
  const store: Record<string, Buffer> = stored ? { 'secret/boxlite/dev.json': sealed(stored) } : {}
  return {
    puts,
    ssm: {
      send: async (command: any) => {
        const name = command.input.Name
        if (name === '/sst/bootstrap') return { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
        if (name === '/sst/passphrase/boxlite/dev') return { Parameter: { Value: KEY.toString('base64') } }
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

test('the named key goes, and every other key stays as it was', async () => {
  const fake = clients({ stored: { PORT: '8080', SMTP_USER: 'alice', OLD_KEY: 'gone' } })
  const result = await deleteValues({ clients: fake, app: 'boxlite', stage: 'dev', names: ['OLD_KEY'] })
  assert.deepEqual(result.outcomes, [{ name: 'OLD_KEY', existed: true }])
  assert.deepEqual(opened(fake.puts[0].Body), { PORT: '8080', SMTP_USER: 'alice' })
})

test('several keys go together, in one write', async () => {
  // The store is one object, so three removals one at a time would be three
  // round trips and three windows for a concurrent writer to lose a change.
  const fake = clients({ stored: { A: '1', B: '2', C: '3', KEPT: 'as it was' } })
  const result = await deleteValues({ clients: fake, app: 'boxlite', stage: 'dev', names: ['A', 'B', 'C'] })
  assert.deepEqual(result.outcomes, [
    { name: 'A', existed: true },
    { name: 'B', existed: true },
    { name: 'C', existed: true },
  ])
  assert.equal(fake.puts.length, 1, 'one read-modify-write, not one per key')
  assert.deepEqual(opened(fake.puts[0].Body), { KEPT: 'as it was' })
})

test('a name that was not there is reported beside the ones that were', async () => {
  const fake = clients({ stored: { A: '1', C: '3' } })
  const result = await deleteValues({ clients: fake, app: 'boxlite', stage: 'dev', names: ['A', 'B', 'C'] })
  assert.deepEqual(result.outcomes, [
    { name: 'A', existed: true },
    { name: 'B', existed: false },
    { name: 'C', existed: true },
  ])
  assert.deepEqual(opened(fake.puts[0].Body), {}, 'the two that were there are gone')
})

test('what is left can be read back through the reader that SST shares', async () => {
  const fake = clients({ stored: { PORT: '8080', OLD_KEY: 'gone' } })
  await deleteValues({ clients: fake, app: 'boxlite', stage: 'dev', names: ['OLD_KEY'] })
  const reread = await readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' })
  assert.deepEqual(reread, { PORT: '8080' }, 'the object must stay in the layout the reader expects')
})

test('removing the last key leaves an object the reader still opens', async () => {
  // An empty store is a store, not a missing one: SST reads `{}` and so must this.
  const fake = clients({ stored: { ONLY: 'v' } })
  await deleteValues({ clients: fake, app: 'boxlite', stage: 'dev', names: ['ONLY'] })
  assert.deepEqual(opened(fake.puts[0].Body), {})
  assert.deepEqual(await readEnvironment({ clients: fake, app: 'boxlite', stage: 'dev' }), {})
})

test('a key that is not there is reported, and the object is not resealed for nothing', async () => {
  const fake = clients({ stored: { PORT: '8080' } })
  const result = await deleteValues({ clients: fake, app: 'boxlite', stage: 'dev', names: ['NEVER_SET'] })
  assert.deepEqual(result.outcomes, [{ name: 'NEVER_SET', existed: false }])
  assert.equal(fake.puts.length, 0, 'a no-op must not rewrite the whole stage')

  const none = clients({ stored: { PORT: '8080' } })
  await deleteValues({ clients: none, app: 'boxlite', stage: 'dev', names: ['NOR_THIS', 'NOR_THAT'] })
  assert.equal(none.puts.length, 0, 'and neither must a whole line of them')
})

test('a name the map only inherits was never set, and reseals nothing', async () => {
  // The map comes out of JSON.parse, so it inherits toString and the rest of
  // Object.prototype. `in` would call those present, report them removed, and
  // rewrite the whole encrypted object for a key no store can hold.
  const fake = clients({ stored: { PORT: '8080' } })
  const result = await deleteValues({
    clients: fake,
    app: 'boxlite',
    stage: 'dev',
    names: ['toString', 'constructor', '__proto__'],
  })
  assert.deepEqual(result.outcomes, [
    { name: 'toString', existed: false },
    { name: 'constructor', existed: false },
    { name: '__proto__', existed: false },
  ])
  assert.equal(fake.puts.length, 0, 'nothing was there, so nothing is resealed')
})

test('a stage that was never written has nothing to remove', async () => {
  const fake = clients()
  const result = await deleteValues({ clients: fake, app: 'boxlite', stage: 'dev', names: ['A'] })
  assert.equal(result.outcomes[0]!.existed, false)
  assert.equal(fake.puts.length, 0)
})


/** A repository that fingerprints `deploy`, whose digest key is a member of it. */
const CONFIG = {
  path: '/repo/mstage.config.json',
  envSelectGroup: { deploy: ['KEPT', 'DIGEST'] },
  envDigest: { key: 'DIGEST', group: 'deploy' },
} as any

const scope = (protect = false) => ({ app: 'boxlite', stage: 'dev', protect }) as any

const remove = ({
  positionals,
  options = {},
  protect = false,
  fake,
  config = CONFIG,
}: {
  positionals: string[]
  options?: Record<string, string | boolean>
  protect?: boolean
  fake: ReturnType<typeof clients>
  config?: any
}) => {
  const lines: string[] = []
  return {
    lines,
    run: () =>
      del({
        config,
        scope: scope(protect),
        positionals,
        options,
        log: (line: string) => lines.push(line),
        backend: fake as any,
      }),
  }
}

test('--digest leaves the fingerprint alone, because a removal it allows cannot change it', async () => {
  // The flag checks; it does not write. A removal that touches no member of the
  // certified group cannot change that group's digest, and one that does touch a
  // member is refused above — so there is no case left for a recompute, and
  // writing one anyway would be a line claiming work nobody did.
  const fake = clients({ stored: { KEPT: 'v', DIGEST: 'as it was', ORPHAN: 'x' } })
  const attempt = remove({ positionals: ['ORPHAN'], options: { digest: true }, fake })
  assert.equal(await attempt.run(), 0)

  const written = opened(fake.puts[0].Body)
  assert.deepEqual(written, { KEPT: 'v', DIGEST: 'as it was' }, 'the fingerprint is untouched, not rewritten')
  assert.match(
    attempt.lines.join('\n'),
    /^# none of these is in env\.selectGroup\.deploy, so DIGEST still describes it$/m,
  )
})

test('--digest refuses a name the group it certifies holds, and removes nothing', async () => {
  // Removing part of a group and certifying that group ask for opposite things;
  // the digest key is itself a member, so both spellings of the mistake refuse.
  for (const name of ['KEPT', 'DIGEST']) {
    const fake = clients({ stored: { KEPT: 'v', DIGEST: 'd', ORPHAN: 'x' } })
    await assert.rejects(
      () => remove({ positionals: [name, 'ORPHAN'], options: { digest: true }, fake }).run(),
      new RegExp(`--digest keeps DIGEST true, and env\\.selectGroup\\.deploy names ${name}`),
    )
    assert.equal(fake.puts.length, 0, 'not even the name outside the group goes')
  }
})

test('without --digest every name goes, group member and digest key alike', async () => {
  // The flag is what asks for a certified group; without it the caller is
  // removing keys and knows what they are doing. The next deploy refuses by name
  // until the group declaration catches up, which is loud and is the point.
  const member = clients({ stored: { KEPT: 'v', DIGEST: 'd' } })
  assert.equal(await remove({ positionals: ['KEPT'], fake: member }).run(), 0)
  assert.deepEqual(opened(member.puts[0].Body), { DIGEST: 'd' })

  const key = clients({ stored: { KEPT: 'v', DIGEST: 'd' } })
  assert.equal(await remove({ positionals: ['DIGEST'], fake: key }).run(), 0)
  assert.deepEqual(opened(key.puts[0].Body), { KEPT: 'v' }, 'the fingerprint itself is removable')
})

test('a repository with no env.digest is told so, and the removal still lands', async () => {
  const fake = clients({ stored: { ORPHAN: 'x' } })
  const attempt = remove({
    positionals: ['ORPHAN'],
    options: { digest: true },
    fake,
    config: { ...CONFIG, envDigest: null },
  })
  assert.equal(await attempt.run(), 0)
  assert.match(attempt.lines[0]!, /declares no env\.digest; there is no fingerprint to keep true/)
  assert.equal(fake.puts.length, 1)
})

test('a removal that removes nothing writes nothing, and says nothing extra', async () => {
  const fake = clients({ stored: { KEPT: 'v', DIGEST: 'd' } })
  const attempt = remove({ positionals: ['NEVER_SET'], options: { digest: true }, fake })
  assert.equal(await attempt.run(), 0)
  assert.equal(fake.puts.length, 0)
  assert.deepEqual(attempt.lines, ['NEVER_SET was not set in boxlite/dev'], 'a store that did not move needs no note')
})

test('a protected stage is not written to without --confirm', async () => {
  const fake = clients({ stored: { PORT: '8080' } })
  await assert.rejects(() => remove({ positionals: ['PORT'], protect: true, fake }).run(), /add --confirm/)
  assert.equal(fake.puts.length, 0)

  const confirmed = remove({ positionals: ['PORT'], options: { confirm: true }, protect: true, fake })
  assert.equal(await confirmed.run(), 0)
  assert.equal(fake.puts.length, 1)
})

test('at least one name is required', async () => {
  const fake = clients({ stored: { PORT: '8080' } })
  await assert.rejects(
    () => remove({ positionals: [], fake }).run(),
    /usage: npm run mstage env del -- KEY \[KEY …\] --stage=<stage>/,
  )
  assert.equal(fake.puts.length, 0)
})

test('an empty name is refused, alone or beside a real one', async () => {
  // `env del -- ""` reaches the handler as one empty positional. The old
  // single-key guard rejected it through `!name`; a length check alone does not,
  // and `del` applies no other naming rule on purpose.
  const alone = clients({ stored: { A: '1' } })
  await assert.rejects(
    () => remove({ positionals: [''], fake: alone }).run(),
    /an empty name is not a key; every name given must be one/,
  )
  assert.equal(alone.puts.length, 0)

  const beside = clients({ stored: { A: '1' } })
  await assert.rejects(() => remove({ positionals: ['A', ''], fake: beside }).run(), /an empty name is not a key/)
  assert.equal(beside.puts.length, 0, 'the real name does not go either')
})

test('three names on the line are three removals and one write', async () => {
  const fake = clients({ stored: { A: '1', B: '2', C: '3', KEPT: 'v' } })
  const attempt = remove({ positionals: ['A', 'B', 'C'], fake })
  assert.equal(await attempt.run(), 0)
  assert.equal(fake.puts.length, 1)
  assert.deepEqual(attempt.lines, [
    'A removed from boxlite/dev',
    'B removed from boxlite/dev',
    'C removed from boxlite/dev',
    '# run a deploy for the change to reach anything running',
  ])
})

test('one name that was never set does not stop the others, and is said so', async () => {
  const fake = clients({ stored: { A: '1', C: '3' } })
  const attempt = remove({ positionals: ['A', 'B', 'C'], fake })
  assert.equal(await attempt.run(), 0)
  assert.deepEqual(attempt.lines.slice(0, 3), [
    'A removed from boxlite/dev',
    'B was not set in boxlite/dev',
    'C removed from boxlite/dev',
  ])
})

test('a name repeated on the line is refused rather than reported twice', async () => {
  const fake = clients({ stored: { A: '1' } })
  await assert.rejects(
    () => remove({ positionals: ['A', 'B', 'A'], fake }).run(),
    /A given more than once; one removal is enough/,
  )
  assert.equal(fake.puts.length, 0)
})

test('what was removed is named; what it held is not', async () => {
  const fake = clients({ stored: { API_TOKEN: 'super-secret' } })
  const attempt = remove({ positionals: ['API_TOKEN'], fake })
  assert.equal(await attempt.run(), 0)
  const printed = attempt.lines.join('\n')
  assert.match(printed, /^API_TOKEN removed from boxlite\/dev$/m)
  assert.match(printed, /run a deploy/)
  assert.ok(!printed.includes('super-secret'), printed)
})

test('the state bucket is not named, on either outcome', async () => {
  // Its twelve random characters are the only thing keeping the state objects
  // from being addressable; a log that carries them hands that away.
  for (const name of ['PORT', 'NEVER_SET']) {
    const attempt = remove({ positionals: [name], fake: clients({ stored: { PORT: '8080' } }) })
    assert.equal(await attempt.run(), 0)
    assert.ok(!attempt.lines.join('\n').includes('sst-state-x'), attempt.lines.join('\n'))
  }
})

test('a key that was not set says so, and does not send anyone off to deploy', async () => {
  const fake = clients({ stored: { PORT: '8080' } })
  const attempt = remove({ positionals: ['NEVER_SET'], fake })
  assert.equal(await attempt.run(), 0)
  assert.deepEqual(attempt.lines, ['NEVER_SET was not set in boxlite/dev'])
})
