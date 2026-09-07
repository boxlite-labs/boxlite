/*
 * What a deploy asks the store for, against this repository's real declaration.
 *
 * The defect this guards: `env.selectGroup.deploy` once listed every key the
 * stack could read, all required, so `valuesOfGroup` refused every deploy over
 * forty keys a stage had no reason to seed — and four it could never seed at
 * all, because an image tag and a runner binary's checksum are different on
 * every run.
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { parseConfig } from 'mstage/config'
import { valuesOfGroup } from 'mstage/select-group'
import { serviceSecretsFrom, splitServiceChannels, type GroupDeclaration } from '../src/env.ts'

const config = (() => {
  const path = new URL('../../mstage.config.json', import.meta.url)
  return parseConfig(path.pathname, readFileSync(path, 'utf8'))
})()

const declaration: GroupDeclaration = {
  groups: config.envSelectGroup,
  optional: config.envOptional,
  where: config.path,
}

/** A store holding the deploy group's required keys and nothing else. */
const minimal = {
  STACK_DOMAIN: 'dev.boxlite.ai',
  PROXY_DOMAIN: 'box.dev.boxlite.ai',
  OIDC_ISSUER_BASE_URL: 'https://auth.dev.boxlite.ai',
  IAM_PERMISSIONS_BOUNDARY_STAGE: 'dev',
  CLOUDFLARE_API_TOKEN: 'cf-token',
  CLOUDFLARE_DEFAULT_ACCOUNT_ID: 'cf-account',
  CLOUDFLARE_ZONE_ID: 'cf-zone',
  BOXLITE_STAGE_CONFIG_DIGEST: 'd'.repeat(64),
}

const narrow = (group: string, values: Record<string, string>) =>
  valuesOfGroup({
    group,
    groups: config.envSelectGroup,
    values,
    where: config.path,
    optional: config.envOptional[group] ?? [],
  })

test('a stage that seeded only the required keys can deploy', () => {
  // The reproducer. Before the split this threw, naming forty keys — including
  // four no store could ever hold.
  const resolved = narrow('deploy', minimal)
  assert.deepEqual(Object.keys(resolved).sort(), Object.keys(minimal).sort())
})

test('a required key the store lacks still stops the deploy, named', () => {
  // The invariant the split must not weaken: a silently short environment fails
  // hours later, somewhere that does not mention the key.
  const { CLOUDFLARE_ZONE_ID, ...without } = minimal
  assert.throws(() => narrow('deploy', without), /the store is missing CLOUDFLARE_ZONE_ID/)
})

test('an optional key the store does hold is carried through', () => {
  // Optional means "may be absent", never "ignored".
  const resolved = narrow('deploy', { ...minimal, BILLING_API_URL: 'https://commerce.example.com' })
  assert.equal(resolved.BILLING_API_URL, 'https://commerce.example.com')
})

test('every service group is satisfiable by its own required half', () => {
  const required = (group: string) =>
    config.envSelectGroup[group]!.filter((key) => !config.envOptional[group]!.includes(key))
  for (const group of ['api', 'proxy', 'otel-collector', 'runner']) {
    const values = Object.fromEntries(required(group).map((key) => [key, 'x']))
    assert.doesNotThrow(() => narrow(group, values), `${group} cannot be satisfied by its required keys alone`)
  }
})

test('a service reads its optional keys when present and boots without them when not', () => {
  // `serviceSecretsFrom` is the delivery side of the same question, and it has
  // to agree with the store side: a key the store was allowed to omit cannot
  // then be demanded of the deploy.
  const withoutOptional = { ADMIN_API_KEY: 'a', ENCRYPTION_KEY: 'b', ENCRYPTION_SALT: 'c', OIDC_CLIENT_ID: 'd' }
  const bare = serviceSecretsFrom({ group: 'api', declaration, environment: withoutOptional })
  assert.deepEqual(Object.keys(bare).sort(), ['ADMIN_API_KEY', 'ENCRYPTION_KEY', 'ENCRYPTION_SALT', 'OIDC_CLIENT_ID'])

  const withOne = serviceSecretsFrom({
    group: 'api',
    declaration,
    environment: { ...withoutOptional, POSTHOG_API_KEY: 'ph' },
  })
  assert.equal(withOne.POSTHOG_API_KEY, 'ph')
})

test('a required service key that never arrived still stops the deploy', () => {
  assert.throws(
    () => serviceSecretsFrom({ group: 'api', declaration, environment: { ADMIN_API_KEY: 'a' } }),
    /ENCRYPTION_KEY, ENCRYPTION_SALT, OIDC_CLIENT_ID must reach the deploy/,
  )
})

test('splitting channels still works when an optional key is simply absent', () => {
  // The split reads the marker group; an absent key must not become an entry
  // with an undefined value, which is what would reach a container as "".
  const { values, addresses } = splitServiceChannels({
    delivered: serviceSecretsFrom({
      group: 'proxy',
      declaration,
      environment: { PROXY_API_KEY: 'k' },
    }),
    declaration,
    home: 'aws',
  })
  assert.deepEqual(values, { PROXY_API_KEY: 'k' })
  assert.deepEqual(addresses, {})
})
