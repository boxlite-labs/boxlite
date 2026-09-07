import assert from 'node:assert/strict'
import { createCipheriv, createDecipheriv, randomBytes } from 'node:crypto'
import test from 'node:test'
import { set } from '../src/cli/handlers/env.ts'

const KEY = randomBytes(32)

const sealed = (value: unknown): Buffer => {
  const nonce = randomBytes(12)
  const cipher = createCipheriv('aes-256-gcm', KEY, nonce)
  const body = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

const opened = (payload: Buffer): Record<string, string> => {
  const nonce = payload.subarray(0, 12)
  const decipher = createDecipheriv('aes-256-gcm', KEY, nonce)
  decipher.setAuthTag(payload.subarray(payload.length - 16))
  const body = payload.subarray(12, payload.length - 16)
  return JSON.parse(Buffer.concat([decipher.update(body), decipher.final()]).toString('utf8'))
}

const notFound = (name: string) => Object.assign(new Error(name), { name })

const ARN = 'arn:aws:ssm:ap-southeast-1:123456789012:parameter/boxlite/dev/oidc-client-secret'
const SECRET_NAME = 'projects/boxlite-dev/secrets/oidc-client-secret'

const JSON_FLAG = { json: true }

/** `env set` with whatever a redirect carried, and nothing else reading stdin. */
const harness = ({
  piped = '',
  stored = { KEPT: 'as it was' } as Record<string, string>,
  groups = { deploy: ['SHIPPED_ONE', 'SHIPPED_TWO'] } as Record<string, string[]>,
  home = 'aws' as 'aws' | 'gcp',
  digest = null as { key: string; group: string } | null,
  typed,
}: {
  piped?: string
  stored?: Record<string, string>
  groups?: Record<string, string[]>
  home?: 'aws' | 'gcp'
  /** What this repository fingerprints, when a test needs `--digest` to mean something. */
  digest?: { key: string; group: string } | null
  /** What a lone `KEY` reads off a file. Absent means nothing may read stdin. */
  typed?: string
} = {}) => {
  const puts: any[] = []
  const lines: string[] = []
  return {
    puts,
    lines,
    written: () => (puts.length > 0 ? opened(puts[puts.length - 1].Body) : null),
    run: (positionals: string[] = [], options: Record<string, string | boolean> = {}) =>
      set({
        config: { path: '/repo/mstage.config.json', home, envSelectGroup: groups, envDigest: digest } as any,
        scope: { app: 'a', stage: 'dev', protect: false } as any,
        positionals,
        options,
        log: (line: string) => lines.push(line),
        backend: {
          s3: {
            send: async (command: any) => {
              if (command.input.Body) {
                puts.push(command.input)
                return {}
              }
              if (command.input.Key !== 'secret/a/dev.json') throw notFound('NoSuchKey')
              return { Body: { transformToByteArray: async () => sealed(stored) } }
            },
          },
          ssm: {
            send: async (command: any) =>
              command.input.Name === '/sst/bootstrap'
                ? { Parameter: { Value: JSON.stringify({ state: 'sst-state-x' }) } }
                : { Parameter: { Value: KEY.toString('base64') } },
          },
        } as any,
        readInput: async () => {
          if (typed === undefined) throw new Error('a batch must not be read one value at a time')
          return typed
        },
        readBatch: async () => piped,
      }),
  }
}

test('a piped JSON object sets every key it names, in one write', async () => {
  const probe = harness({ piped: JSON.stringify({ ONE: '1', TWO: '2' }) })
  assert.equal(await probe.run(), 0)
  assert.equal(probe.puts.length, 1, 'a document is one change, so it is one write')
  assert.deepEqual(probe.written(), { KEPT: 'as it was', ONE: '1', TWO: '2' })
  assert.deepEqual(probe.lines, [
    'ONE added in a/dev',
    'TWO added in a/dev',
    '# run a deploy for the change to reach anything running',
  ])
})

test('a value with commas in it is one string, in and out', async () => {
  // What `env list --json` prints is what loads back in, so a store exported
  // from one stage goes into another unedited — and `KEY=A,B,C,D` is the string
  // it looks like rather than a list somebody has to know about.
  const exported = { ONE: 'a', MANY: 'x,y,z' }
  const probe = harness({ piped: JSON.stringify(exported), stored: {} })
  await probe.run()
  assert.deepEqual(probe.written(), exported, 'what went in is what comes back out')
})

test('a value stored as JSON text is named, because an old export holds arrays that were values', async () => {
  // The tool documents `env list --json > file` as the way to move a stage. A
  // file written by the version that split a comma-holding value into an array
  // reloads here as the text of that array — a rewrite, and one nobody may
  // discover unless this line says which keys it touched.
  const probe = harness({ piped: JSON.stringify({ WAS_A_VALUE: ['a', 'b'], PLAIN: 'x' }), stored: {} })
  assert.equal(await probe.run(), 0)
  const printed = probe.lines.join('\n')
  assert.match(printed, /^# stored as the JSON text of a list: WAS_A_VALUE\. An export written before/m)
  assert.ok(!printed.includes('PLAIN,'), 'a string value is stored as it arrived, so it is not named here')
})

test('a document of strings alone says nothing extra', async () => {
  const probe = harness({ piped: JSON.stringify({ ONE: '1' }), stored: {} })
  await probe.run()
  assert.ok(!probe.lines.join('\n').includes('stored as the JSON text'))
})

test('an object or a number is not named, because no export ever produced one', async () => {
  // The notice is about a list, which the older `env list --json` could put
  // where a comma-holding value had been. The documented nested forms are what
  // their author wrote, and saying otherwise about them would be false.
  const probe = harness({ piped: JSON.stringify({ KA: { a: 'b' }, PORT: 8080, YES: true }), stored: {} })
  assert.equal(await probe.run(), 0)
  assert.ok(
    !probe.lines.join('\n').includes('stored as the JSON text of a list'),
    'the legacy notice must not claim these were ever comma-joined values',
  )
})

test('--digest does not read a piped document, so one command cannot rewrite and certify', async () => {
  // A list in a document becomes the text of a list — a rewrite, where the file
  // came from the older `env list --json`. The fingerprint that would vouch for
  // it is never computed in the same invocation, because --digest does not wait
  // on stdin at all. Doing both takes two commands, deliberately.
  const probe = harness({
    piped: JSON.stringify({ CERTIFIED: ['a', 'b'] }),
    stored: { CERTIFIED: 'a,b', DIGEST: 'as it was' },
    groups: { deploy: ['CERTIFIED', 'DIGEST'] },
    digest: { key: 'DIGEST', group: 'deploy' },
  })
  assert.equal(await probe.run([], { digest: true }), 0)
  assert.equal(probe.written()!.CERTIFIED, 'a,b', 'the document was not read, so nothing was rewritten')
})

test('a piped list is stored and named when no fingerprint is being written', async () => {
  const probe = harness({
    piped: JSON.stringify({ OUTSIDE: ['a', 'b'] }),
    stored: {},
    groups: { deploy: ['CERTIFIED', 'DIGEST'] },
    digest: { key: 'DIGEST', group: 'deploy' },
  })
  assert.equal(await probe.run(), 0)
  assert.equal(probe.written()!.OUTSIDE, '["a","b"]')
  assert.match(probe.lines.join('\n'), /^# stored as the JSON text of a list: OUTSIDE\./m)
})

test('a nested document is stored as its JSON text, so a file needs no escaping', async () => {
  // The shape that arrives from an editor: one key holding an object, another
  // holding a string. Both land, and the object lands as JSON writes it.
  const probe = harness({ piped: JSON.stringify({ KA: { A: 'one', C: 'two' }, KB: 'plain' }), stored: {} })
  assert.equal(await probe.run(), 0)
  assert.deepEqual(probe.written(), { KA: '{"A":"one","C":"two"}', KB: 'plain' })
})

test('a list is stored verbatim as a list, not joined on a separator', async () => {
  const probe = harness({ piped: JSON.stringify({ MANY: ['x', 'y'] }), stored: {} })
  assert.equal(await probe.run(), 0)
  assert.deepEqual(probe.written(), { MANY: '["x","y"]' }, 'its own elements may contain any separator')
})

test('a number and a boolean are stored as the text JSON writes for them', async () => {
  const probe = harness({ piped: JSON.stringify({ PORT: 8080, ENABLED: true }), stored: {} })
  assert.equal(await probe.run(), 0)
  assert.deepEqual(probe.written(), { PORT: '8080', ENABLED: 'true' })
})

test('a document a command line could not carry survives the write and the read', async () => {
  const nested = { pem: 'line one\nline two', list: [1, 2] }
  const probe = harness({ piped: JSON.stringify({ CONFIG: nested }), stored: {} })
  await probe.run()
  assert.deepEqual(JSON.parse(probe.written()!.CONFIG!), nested, 'the document is the value, unchanged')
})

test('an address can arrive nested, and is checked like any other', async () => {
  const probe = harness({
    piped: JSON.stringify({ OIDC_CLIENT_SECRET: { address: ARN } }),
    stored: {},
    groups: { secret: ['OIDC_CLIENT_SECRET'] },
  })
  assert.equal(await probe.run(), 0)
  assert.deepEqual(probe.written(), { OIDC_CLIENT_SECRET: JSON.stringify({ address: ARN }) })

  const wrong = harness({
    piped: JSON.stringify({ OIDC_CLIENT_SECRET: { arn: ARN } }),
    stored: {},
    groups: { secret: ['OIDC_CLIENT_SECRET'] },
  })
  await assert.rejects(() => wrong.run(), /OIDC_CLIENT_SECRET names arn, which an address has no field for/)
  assert.equal(wrong.puts.length, 0)
})

test('JSON carries a newline itself, and no second round of unescaping', async () => {
  const probe = harness({ piped: JSON.stringify({ PEM: 'a\nb', LITERAL: 'a\\nb' }), stored: {} })
  await probe.run()
  const written = probe.written()!
  assert.equal(written.PEM, 'a\nb')
  assert.equal(written.PEM!.split('\n').length, 2)
  // `KEY=a\nb` on the command line would expand; a JSON value is already final.
  assert.equal(written.LITERAL, 'a\\nb')
})

test('a null value is the one refusal, because a stored "null" is nobody\'s intention', async () => {
  const probe = harness({ piped: JSON.stringify({ BAD: null, GOOD: 'kept' }) })
  await assert.rejects(() => probe.run(), /^Error: BAD is null; remove a key with env del, or give it a value$/)
  assert.equal(probe.puts.length, 0, 'and the rest of the document does not land either')
})

test('a document that is not an object, or not JSON at all, says which', async () => {
  await assert.rejects(() => harness({ piped: '["A", "B"]' }).run(), /must hold a JSON object of names to values/)
  await assert.rejects(() => harness({ piped: 'KEY=VALUE' }).run(), /stdin does not hold valid JSON/)
})

test('nothing piped and nothing typed is a usage error naming all three forms', async () => {
  const probe = harness({ piped: '   \n' })
  await assert.rejects(() => probe.run(), /KEY=VALUE .*or pipe a JSON object of them, or --digest alone/)
  assert.equal(probe.puts.length, 0)
})

test('--select-group keeps the keys one group names and says which it dropped', async () => {
  const probe = harness({
    piped: JSON.stringify({ SHIPPED_ONE: 'yes', SECRET_TOKEN: 'hunter2', SHIPPED_TWO: 'also' }),
    stored: {},
  })
  assert.equal(await probe.run([], { 'select-group': 'deploy' }), 0)
  assert.deepEqual(probe.written(), { SHIPPED_ONE: 'yes', SHIPPED_TWO: 'also' })
  const printed = probe.lines.join('\n')
  assert.match(printed, /^# outside env\.selectGroup\.deploy, not set: SECRET_TOKEN$/m)
  assert.ok(!printed.includes('hunter2'), 'a dropped key is named, never quoted')
})

test('--select-group narrows arguments too, not only a piped document', async () => {
  const probe = harness({ stored: {} })
  await probe.run(['SHIPPED_ONE=yes', 'OUTSIDE=no'], { 'select-group': 'deploy' })
  assert.deepEqual(probe.written(), { SHIPPED_ONE: 'yes' })
})

test('--select-group that keeps nothing refuses rather than reporting a write it did not make', async () => {
  const probe = harness({ piped: JSON.stringify({ OUTSIDE: 'no', ALSO_OUTSIDE: 'no' }) })
  await assert.rejects(() => probe.run([], { 'select-group': 'deploy' }), /none of the 2 keys given are in env\.selectGroup\.deploy/)
  assert.equal(probe.puts.length, 0)
})

test('--select-group naming a group the config does not declare lists the ones it does', async () => {
  const probe = harness({ piped: JSON.stringify({ SHIPPED_ONE: 'yes' }) })
  await assert.rejects(() => probe.run([], { 'select-group': 'nope' }), /declares no "nope" under env\.selectGroup\. Declared: deploy/)
  assert.equal(probe.puts.length, 0)
})

test('--json reads a value as a document, and stores it as JSON writes it', async () => {
  const probe = harness({ stored: {} })
  assert.equal(await probe.run(['SHAPE={"a":"b", "c":"d"}'], JSON_FLAG), 0)
  assert.deepEqual(probe.written(), { SHAPE: '{"a":"b","c":"d"}' })

  // One value, one stored string: the same object typed with other spacing is
  // not a change, so it does not rewrite the object or move the group digest.
  const again = harness({ stored: { SHAPE: '{"a":"b","c":"d"}' } })
  assert.equal(await again.run(['SHAPE={ "a" : "b" , "c" : "d" }'], JSON_FLAG), 0)
  assert.equal(again.puts.length, 0)
  assert.match(again.lines[0]!, /^SHAPE already set to that value in a\/dev$/)
})

test('one document says one thing, whichever of the two ways it arrives', async () => {
  // The rule is shared rather than written twice, because two rules would agree
  // for an object and part company for a string — the shape most likely to be
  // typed by hand, and least likely to be noticed coming back with its quotes.
  for (const [typed, stored] of [
    ['"a"', 'a'],
    ['{"a":"b"}', '{"a":"b"}'],
    ['["a","b"]', '["a","b"]'],
    ['8080', '8080'],
  ] as const) {
    const argument = harness({ stored: {} })
    assert.equal(await argument.run([`KEY=${typed}`], JSON_FLAG), 0)
    assert.deepEqual(argument.written(), { KEY: stored }, `--json KEY=${typed}`)

    const piped = harness({ stored: {}, piped: `{"KEY": ${typed}}` })
    assert.equal(await piped.run(), 0)
    assert.deepEqual(piped.written(), { KEY: stored }, `piped {"KEY": ${typed}}`)

    const file = harness({ stored: {}, typed })
    assert.equal(await file.run(['KEY'], JSON_FLAG), 0)
    assert.deepEqual(file.written(), { KEY: stored }, `--json KEY < file holding ${typed}`)
  }
})

test('null is refused on every one of those ways, not only the piped one', async () => {
  const argument = harness({ stored: {} })
  await assert.rejects(() => argument.run(['KEY=null'], JSON_FLAG), /KEY is null; remove a key with env del/)
  assert.equal(argument.puts.length, 0)

  const file = harness({ stored: {}, typed: 'null' })
  await assert.rejects(() => file.run(['KEY'], JSON_FLAG), /KEY is null; remove a key with env del/)
  assert.equal(file.puts.length, 0)
})

test('without --json a value is a line of text, whatever it begins with', async () => {
  // The reason the reading is asked for and not detected: `{VALUE}` is two
  // words and a pair of braces, not a document that failed to parse.
  const probe = harness({ stored: {} })
  assert.equal(await probe.run(['KEY={VALUE}', 'SHAPE={ "a" : "b" }']), 0)
  assert.deepEqual(probe.written(), { KEY: '{VALUE}', SHAPE: '{ "a" : "b" }' })
})

test("a document's own escapes are the value, not a second round of expansion", async () => {
  const probe = harness({ stored: {} })
  await probe.run(['PEM={"key":"a\\nb"}'], JSON_FLAG)
  const written = probe.written()!.PEM!
  assert.deepEqual(JSON.parse(written), { key: 'a\nb' }, 'the escape is JSON of its own, so it survives as one')
  assert.ok(!written.includes('\n'), 'expanding first leaves a raw newline inside a JSON string, which is not JSON')
})

test('a --json value that does not parse is refused at the write, and never quoted back', async () => {
  const probe = harness({ stored: {} })
  await assert.rejects(
    () => probe.run(['SHAPE={"a": hunter2}'], JSON_FLAG),
    (error: Error) => {
      assert.match(error.message, /SHAPE was given with --json, so its value must be a JSON document/)
      assert.ok(!error.message.includes('hunter2'), 'a refusal must not put the value in scrollback')
      return true
    },
  )
  assert.equal(probe.puts.length, 0)
})

test('a value that is not a document still gets the escapes a shell cannot type', async () => {
  const probe = harness({ stored: {} })
  await probe.run(['TWO=a\\nb'])
  assert.equal(probe.written()!.TWO, 'a\nb')
})

test('--json describes a value, so a line that carries none is refused rather than ignored', async () => {
  const piped = harness({ piped: JSON.stringify({ ONE: '1' }) })
  await assert.rejects(() => piped.run([], JSON_FLAG), /a piped document is already JSON/)
  assert.equal(piped.puts.length, 0)

  const digestOnly = harness()
  await assert.rejects(() => digestOnly.run([], { json: true, digest: true }), /this line gives none/)
  assert.equal(digestOnly.puts.length, 0)
})

test('--json reads a file as a document too, which is how a large one arrives', async () => {
  // A document too big for a command line comes off a file, and it is the same
  // value either way: parsed, and stored as JSON writes it.
  const probe = harness({ stored: {}, typed: '{\n  "a": "b",\n  "c": "d"\n}\n' })
  assert.equal(await probe.run(['PLATFORM_CONFIG'], JSON_FLAG), 0)
  assert.deepEqual(probe.written(), { PLATFORM_CONFIG: '{"a":"b","c":"d"}' })
})

test('--json refuses a file that is not a document, and writes nothing', async () => {
  const probe = harness({ stored: {}, typed: 'not a document at all\n' })
  await assert.rejects(
    () => probe.run(['PLATFORM_CONFIG'], JSON_FLAG),
    /PLATFORM_CONFIG was given with --json, so its value must be a JSON document/,
  )
  assert.equal(probe.puts.length, 0)
})

test("without --json a file's bytes are stored as they came, document or not", async () => {
  // The trailing newline included, which is what `sst secret set` stores for
  // the same input.
  const probe = harness({ stored: {}, typed: '{ "a": "b" }\n' })
  assert.equal(await probe.run(['PLATFORM_CONFIG']), 0)
  assert.deepEqual(probe.written(), { PLATFORM_CONFIG: '{ "a": "b" }\n' })
})

test('a key in env.selectGroup.secret takes the address of a secret', async () => {
  const probe = harness({ stored: {}, groups: { secret: ['OIDC_CLIENT_SECRET'] } })
  assert.equal(await probe.run([`OIDC_CLIENT_SECRET={"address": "${ARN}"}`], JSON_FLAG), 0)
  assert.deepEqual(probe.written(), { OIDC_CLIENT_SECRET: JSON.stringify({ address: ARN }) })
})

test('an address is checked whether or not --json read it, because the store is what is checked', async () => {
  // --json decides how the value is read; env.selectGroup.secret decides what it
  // has to be. A well-formed address typed without the flag is stored as typed.
  const probe = harness({ stored: {}, groups: { secret: ['OIDC_CLIENT_SECRET'] } })
  assert.equal(await probe.run([`OIDC_CLIENT_SECRET={"address":"${ARN}"}`]), 0)
  assert.deepEqual(probe.written(), { OIDC_CLIENT_SECRET: `{"address":"${ARN}"}` })
})

test('the secret itself is refused where its address belongs, before any write', async () => {
  const probe = harness({ stored: {}, groups: { secret: ['OIDC_CLIENT_SECRET'] } })
  await assert.rejects(
    // Without --json, so what refuses it is the store's own contract for that
    // key rather than the parse the flag asks for.
    () => probe.run(['OIDC_CLIENT_SECRET=hunter2']),
    (error: Error) => {
      assert.equal(error.name, 'EnvError')
      assert.match(error.message, /^OIDC_CLIENT_SECRET does not hold JSON, and a key in env\.selectGroup\.secret/)
      assert.ok(!error.message.includes('hunter2'), 'the one value that must never be echoed is this one')
      return true
    },
  )
  assert.equal(probe.puts.length, 0)
})

test('a key outside the group is not read as an address, however it is spelled', async () => {
  const probe = harness({ stored: {}, groups: { secret: ['OIDC_CLIENT_SECRET'] } })
  await probe.run(['OTHER=hunter2'])
  assert.deepEqual(probe.written(), { OTHER: 'hunter2' })
})

test("a GCP stage's store takes a Secret Manager name, and an ARN is the wrong cloud", async () => {
  const gcp = harness({ stored: {}, groups: { secret: ['OIDC_CLIENT_SECRET'] }, home: 'gcp' })
  assert.equal(await gcp.run([`OIDC_CLIENT_SECRET={"address": "${SECRET_NAME}"}`], JSON_FLAG), 0)
  assert.deepEqual(gcp.written(), { OIDC_CLIENT_SECRET: JSON.stringify({ address: SECRET_NAME }) })

  const wrongCloud = harness({ stored: {}, groups: { secret: ['OIDC_CLIENT_SECRET'] }, home: 'gcp' })
  await assert.rejects(
    () => wrongCloud.run([`OIDC_CLIENT_SECRET={"address": "${ARN}"}`], JSON_FLAG),
    /OIDC_CLIENT_SECRET does not name a Secret Manager secret name/,
  )
  assert.equal(wrongCloud.puts.length, 0)
})
