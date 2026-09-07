import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import test from 'node:test'
import { compareDigest, digestOf, digestOfGroup } from '../src/env/digest.ts'
import { parseAssignment, unescape } from '../src/cli/handlers/env.ts'
import { ConfigError, parseConfig } from '../src/config/load.ts'

/**
 * The formula `apps/api/src/sst-environment.store.ts:49-57` already uses. Both
 * sides must agree or the comparison is meaningless, so this restates it rather
 * than calling the implementation.
 */
const reference = (values: Record<string, string>) => {
  const hash = createHash('sha256')
  for (const key of Object.keys(values).sort()) {
    const value = values[key] ?? ''
    hash.update(`${key.length}:${key}=${value.length}:${value}\n`)
  }
  return hash.digest('hex')
}

test('the digest matches the formula the API already computes', () => {
  const values = { BACKOFFICE_DOMAIN: 'example.com', PORT: '8080', EMPTY: '' }
  assert.equal(digestOf(values), reference(values))
})

test('key order does not change the digest, but content does', () => {
  assert.equal(digestOf({ A: '1', B: '2' }), digestOf({ B: '2', A: '1' }))
  assert.notEqual(digestOf({ A: '1' }), digestOf({ A: '2' }))
})

test('the lengths in the formula stop two different stores colliding', () => {
  // Without them `A=1,B=` and `A=1,B` would hash the same bytes.
  assert.notEqual(digestOf({ A: '1', B: '' }), digestOf({ 'A=1': '', B: '' }))
  assert.notEqual(digestOf({ AB: 'c' }), digestOf({ A: 'Bc' }))
})

test('the digest excludes the key it is written to', () => {
  const withDigest = { A: '1', D: 'whatever' }
  assert.equal(digestOfGroup({ values: withDigest, digestKey: 'D' }), digestOf({ A: '1' }))
  // Otherwise the value would depend on itself and never settle.
  assert.equal(digestOfGroup({ values: { A: '1', D: 'other' }, digestKey: 'D' }), digestOf({ A: '1' }))
})

test('a comparison reports both sides so a mismatch is readable', () => {
  const expected = digestOf({ A: '1' })
  assert.deepEqual(compareDigest({ values: { A: '1', D: expected }, digestKey: 'D' }), {
    expected,
    stored: expected,
    matches: true,
  })
  const stale = compareDigest({ values: { A: '2', D: expected }, digestKey: 'D' })
  assert.equal(stale.matches, false)
  assert.equal(stale.stored, expected)
  assert.notEqual(stale.expected, expected)
})

test('an unset digest key compares as absent rather than as empty', () => {
  const comparison = compareDigest({ values: { A: '1' }, digestKey: 'D' })
  assert.equal(comparison.stored, null)
  assert.equal(comparison.matches, false)
})

const config = (env: unknown) =>
  parseConfig(
    '/repo/mstage.config.json',
    JSON.stringify({ app: 'a', home: 'aws', env, stages: { dev: { region: 'ap-southeast-1' } } }),
  )

test('env.digest names a key that its group must carry', () => {
  const parsed = config({ selectGroup: { deploy: ['A', 'D'] }, digest: { key: 'D', group: 'deploy' } })
  assert.deepEqual(parsed.envDigest, { key: 'D', group: 'deploy' })
  // The digest travels with the group it describes.
  assert.throws(
    () => config({ selectGroup: { deploy: ['A'] }, digest: { key: 'D', group: 'deploy' } }),
    /env\.selectGroup\.deploy must include D/,
  )
})

test('env.digest cannot name a group env.selectGroup does not declare', () => {
  assert.throws(() => config({ selectGroup: { deploy: ['D'] }, digest: { key: 'D', group: 'other' } }), ConfigError)
  assert.throws(
    () => config({ selectGroup: { deploy: ['D'] }, digest: { key: 'D', group: 'other' } }),
    /which env\.selectGroup does not declare/,
  )
})

test('the group defaults to deploy, and an absent block means no digest', () => {
  assert.deepEqual(config({ selectGroup: { deploy: ['D'] }, digest: { key: 'D' } }).envDigest, { key: 'D', group: 'deploy' })
  assert.equal(config({ selectGroup: { deploy: ['A'] } }).envDigest, null)
})

test('an assignment splits on the first "=" and expands escapes', () => {
  assert.deepEqual(parseAssignment('PORT=8080'), ['PORT', '8080'])
  assert.deepEqual(parseAssignment('B64=aGVsbG8='), ['B64', 'aGVsbG8='])
  assert.deepEqual(parseAssignment('EMPTY='), ['EMPTY', ''])
  assert.deepEqual(parseAssignment('TWO=a\\nb'), ['TWO', 'a\nb'])
})

test('an argument with no "=" is not an assignment', () => {
  assert.throws(() => parseAssignment('PORT'), /"PORT" is not a KEY=VALUE assignment/)
  assert.throws(() => parseAssignment('=novalue'), /is not a KEY=VALUE assignment/)
})

test('escapes cover what a shell cannot express, and leave the rest alone', () => {
  assert.equal(unescape('a\\nb'), 'a\nb')
  assert.equal(unescape('a\\tb'), 'a\tb')
  assert.equal(unescape('a\\rb'), 'a\rb')
  assert.equal(unescape('a\\\\nb'), 'a\\nb', 'a literal backslash-n is written \\\\n')
  assert.equal(unescape('C:\\path'), 'C:\\path', 'an unknown escape is left as written')
})
