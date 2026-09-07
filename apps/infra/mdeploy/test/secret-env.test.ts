/*
 * How a Cloud Run container is handed a secret by reference.
 *
 * `containerEnvironment` builds one list where ECS takes two arrays, so the
 * thing worth checking is that the two channels stay distinct inside it: a
 * value arrives with `value`, an address with `valueSource.secretKeyRef`, and
 * neither is ever rendered as the other.
 */

import assert from 'node:assert/strict'
import { readFileSync, readdirSync } from 'node:fs'
import { join } from 'node:path'
import test from 'node:test'
import { fileURLToPath } from 'node:url'
import { containerEnvironment, splitSecretRef } from '../stack/providers/gcp/secret-env.ts'

const REFERENCE = 'projects/boxlite-gcp-dev/secrets/boxlite-dev-db-password/versions/4'

/** The globals the Pulumi engine installs, as much of them as this module reads. */
const installGlobals = () => {
  const target = globalThis as Record<string, unknown>
  target.$util = { output: (value: unknown) => ({ apply: (fn: any) => fn(value), __output: value }) }
  target.$resolve = (values: unknown[]) => ({ apply: (fn: any) => fn(values), __output: values })
}
installGlobals()

const read = (value: any): any => value.__output ?? value

test('a version reference is split into the two halves Cloud Run wants', () => {
  assert.deepEqual(splitSecretRef(REFERENCE), { secret: 'boxlite-dev-db-password', version: '4' })
  assert.deepEqual(splitSecretRef('projects/p/secrets/s/versions/latest'), { secret: 's', version: 'latest' })
})

test('a stored address carries no version, and resolves the latest one', () => {
  // The reproducer. mstage's own validator *refuses* a version on a stored
  // address, so every secret the `secret` marker group delivers arrives in this
  // shape — and a parser that demanded one threw on all of them, on the one
  // channel whose whole point is that the value never travels.
  assert.deepEqual(splitSecretRef('projects/boxlite-gcp-dev/secrets/boxlite-dev-oidc-client-secret'), {
    secret: 'boxlite-dev-oidc-client-secret',
    version: 'latest',
  })
})

test('the shape mstage accepts for a stored address is a shape this parses', () => {
  // One convention rather than two. Both land in the same Cloud Run field, so a
  // form mstage would write and this would reject is a deploy that fails on a
  // string neither side thinks is wrong. Read from mstage's own validator rather
  // than restated here — a copy is what lets the two drift.
  const source = readFileSync(fileURLToPath(new URL('../../mstage/src/env/secret-address.ts', import.meta.url)), 'utf8')
  const declared = /gcp:\s*\{\s*pattern:\s*(\/.+?\/),/s.exec(source)
  assert.ok(declared, 'mstage no longer declares a gcp address pattern where this test looks for it')

  const pattern = new RegExp(declared[1]!.slice(1, -1))
  const address = 'projects/boxlite-gcp-dev/secrets/boxlite-dev-oidc-client-secret'
  assert.match(address, pattern, 'the fixture below has to be an address mstage would accept')
  assert.doesNotThrow(() => splitSecretRef(address))
  // And the reverse: mstage refuses a version, so this must not require one.
  assert.doesNotMatch(`${address}/versions/4`, pattern)
})

test('a string that is not a reference is refused, because it is a plaintext secret', () => {
  // Delivering it anyway would put the secret into the revision as if it named
  // one — the exact failure the reference channel exists to prevent.
  for (const wrong of ['arn:aws:secretsmanager:::secret:x', 'hunter2', 'projects/p/secrets', '']) {
    assert.throws(() => splitSecretRef(wrong), /is not a Secret Manager reference/)
  }
})

test('no provider spells a secret reference itself; they all go through the one builder', () => {
  // The class rather than its instances. The shape written out at each call site
  // is how it goes wrong four times before anyone notices — which is exactly how
  // it went wrong upstream.
  //
  // Recursive: a provider moved into a subdirectory is still a provider, and a
  // guard that read only the top level would stop guarding the day one moves.
  const bundle = fileURLToPath(new URL('../stack/providers/gcp', import.meta.url))
  const offenders: string[] = []
  const walk = (directory: string) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const full = join(directory, entry.name)
      if (entry.isDirectory()) {
        walk(full)
        continue
      }
      if (!entry.name.endsWith('.ts') || entry.name === 'secret-env.ts') continue
      const source = readFileSync(full, 'utf8')
      // Code, not prose: a doc comment may name the field it is describing.
      const code = source
        .split('\n')
        .filter((line) => !/^\s*(\*|\/\/|\/\*)/.test(line))
        .join('\n')
      if (/secretKeyRef/.test(code) || /secrets\\\/\(\[\^\/\]\+\)/.test(code)) offenders.push(entry.name)
    }
  }
  walk(bundle)
  assert.deepEqual(offenders, [], 'these build a secret reference by hand instead of through secret-env.ts')
})

test('values and addresses arrive in one list and stay distinguishable inside it', () => {
  const entries = read(
    containerEnvironment({ values: { OIDC_AUDIENCE: 'boxlite' }, addresses: { DB_PASSWORD: REFERENCE } }),
  )
  assert.deepEqual(entries, [
    { name: 'OIDC_AUDIENCE', value: 'boxlite' },
    {
      name: 'DB_PASSWORD',
      valueSource: { secretKeyRef: { secret: 'boxlite-dev-db-password', version: '4' } },
    },
  ])
})

test('a container with no addresses is not asked to resolve anything', () => {
  const entries = read(containerEnvironment({ values: { PORT: '3000' }, addresses: {} }))
  assert.deepEqual(entries, [{ name: 'PORT', value: '3000' }])
  assert.ok(entries.every((entry: any) => entry.valueSource === undefined))
})

test('no address is ever rendered as a value', () => {
  // The whole point of the channel: the secret never enters the revision. What
  // enters it is the reference, and only in `valueSource`.
  const entries = read(containerEnvironment({ values: {}, addresses: { DB_PASSWORD: REFERENCE } }))
  assert.equal(entries[0].value, undefined)
  assert.equal(JSON.stringify(entries).includes('"value"'), false)
})
