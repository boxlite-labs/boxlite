import assert from 'node:assert/strict'
import test from 'node:test'
import { ExportError, valuesOfGroup } from '../src/env/select-group.ts'
import { ConfigError, parseConfig } from '../src/config/load.ts'

const config = (env: unknown) =>
  parseConfig(
    '/repo/mstage.config.json',
    JSON.stringify({ app: 'a', home: 'aws', env, stages: { dev: { region: 'ap-southeast-1' } } }),
  )

test('groups are read from env.selectGroup, and an absent block is simply no groups', () => {
  const parsed = config({ selectGroup: { deploy: ['DOMAIN', 'IMAGE_TAG'], server: ['SMTP_USER'] } })
  assert.deepEqual(parsed.envSelectGroup, { deploy: ['DOMAIN', 'IMAGE_TAG'], server: ['SMTP_USER'] })
  assert.deepEqual(
    parseConfig('/repo/mstage.config.json', JSON.stringify({ app: 'a', home: 'aws', stages: { dev: {} } })).envSelectGroup,
    {},
  )
})

test('a group that names something that is not a key is refused at load', () => {
  assert.throws(() => config({ selectGroup: { deploy: ['lower_case'] } }), ConfigError)
  assert.throws(() => config({ selectGroup: { deploy: ['has-dash'] } }), /is not a key name/)
  assert.throws(() => config({ selectGroup: { deploy: [42] } }), /is not a key name/)
  assert.throws(() => config({ selectGroup: { deploy: 'A' } }), /must be an array of key names, or an object/)
})

test('a group may separate what the store must hold from what a stage may skip', () => {
  // The two are different statements. A missing required key is the silently
  // short environment `valuesOfGroup` refuses on purpose; a missing optional one
  // is a feature this stage did not configure, and the consumer has an answer
  // for it. One list cannot say both, and forcing the second into the first
  // means seeding a row of empty strings per stage to say nothing at all.
  const parsed = config({ selectGroup: { deploy: { required: ['A'], optional: ['B'] } } })
  assert.deepEqual(parsed.envSelectGroup.deploy, ['A', 'B'], 'a group still names every key it names')
  assert.deepEqual(parsed.envOptional.deploy, ['B'])

  // The short form still means every key is required.
  assert.deepEqual(config({ selectGroup: { deploy: ['A'] } }).envOptional.deploy, [])
})

test('a key cannot be both required and optional', () => {
  // Left in, the required list would win and the optional list would read as a
  // promise the store never made.
  assert.throws(
    () => config({ selectGroup: { deploy: { required: ['A'], optional: ['A'] } } }),
    /names A as both required and optional/,
  )
  assert.throws(
    () => config({ selectGroup: { deploy: { required: ['A'], soptional: ['B'] } } }),
    /does not take soptional/,
  )
})

test('a group with no keys is a declaration, not a mistake', () => {
  // One group per service means a service that reads nothing yet still has to
  // be nameable. Refusing an empty group would force a placeholder key, or no
  // declaration at all — and the second is how a service goes unnoticed.
  assert.deepEqual(config({ selectGroup: { deploy: ['A'], console: [] } }).envSelectGroup, {
    deploy: ['A'],
    console: [],
  })
})

test('a repeated key in one group is a mistake worth naming', () => {
  assert.throws(() => config({ selectGroup: { deploy: ['A', 'B', 'A'] } }), /repeats A/)
})

test('a marked key must be delivered by some group, since the mark names no consumer', () => {
  // `secret` says a key holds an address; it does not say who reads it. A key
  // marked and named nowhere else is an address the store keeps for nobody.
  assert.throws(
    () => config({ selectGroup: { api: ['SMTP_PASSWORD'], secret: ['API_KEY'] } }),
    /"env.selectGroup.secret" marks API_KEY, which no other group names/,
  )
  // Marked and delivered is the whole point of the pair, so it is legal: the
  // service group says who reads it, the mark says how it travels.
  assert.deepEqual(config({ selectGroup: { api: ['API_KEY'], secret: ['API_KEY'] } }).envSelectGroup, {
    api: ['API_KEY'],
    secret: ['API_KEY'],
  })
})

test('a group selects exactly its keys, in the order it declares them', () => {
  const selected = valuesOfGroup({
    group: 'deploy',
    groups: { deploy: ['DOMAIN', 'IMAGE_TAG'] },
    values: { IMAGE_TAG: 'sha', DOMAIN: 'example.com', SMTP_PASSWORD: 'secret' },
    where: '/repo/mstage.config.json',
  })
  assert.deepEqual(Object.keys(selected), ['DOMAIN', 'IMAGE_TAG'], 'declaration order, not store order')
  assert.ok(!('SMTP_PASSWORD' in selected), 'a key outside the group must not leave the store')
})

test('a key the group names but the store lacks fails rather than exporting short', () => {
  // A deploy handed a silently short environment fails somewhere that does not
  // mention the missing key.
  assert.throws(
    () =>
      valuesOfGroup({
        group: 'deploy',
        groups: { deploy: ['DOMAIN', 'MISSING_ONE', 'ALSO_MISSING'] },
        values: { DOMAIN: 'example.com' },
        where: '/repo/mstage.config.json',
      }),
    /store is missing MISSING_ONE, ALSO_MISSING, which env\.selectGroup\.deploy names/,
  )
})

test('an undeclared group lists the declared ones', () => {
  assert.throws(
    () => valuesOfGroup({ group: 'nope', groups: { deploy: ['A'] }, values: {}, where: '/repo/c.json' }),
    /declares no "nope" under env\.selectGroup\. Declared: deploy/,
  )
  assert.throws(() => valuesOfGroup({ group: 'deploy', groups: {}, values: {}, where: '/repo/c.json' }), ExportError)
})

test('a group selects a value whole, commas and all', () => {
  const selected = valuesOfGroup({
    group: 'deploy',
    groups: { deploy: ['LIST'] },
    values: { LIST: 'x,y' },
    where: '/repo/c.json',
  })
  assert.deepEqual(selected, { LIST: 'x,y' }, 'a stored value is a string, whatever is inside it')
})
