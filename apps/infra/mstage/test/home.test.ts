/*
 * The one place a cloud is chosen.
 *
 * What is worth checking without either cloud: that the declaration in
 * `mstage.config.json` is what decides, that an AWS repository never loads
 * Google's SDKs, and that a GCP stage missing the project it is pinned to is
 * refused before anything is reached for.
 *
 * This repository has adopted GCP, so the default factory now builds real
 * clients rather than reporting which packages are absent — the case that used
 * to be asserted here, and no longer describes anything.
 */

import assert from 'node:assert/strict'
import test from 'node:test'
import { resolveHome, type GoogleFactory } from '../src/home.ts'
import type { Scope } from '../src/aws/precedence.ts'

const scope = (overrides: Partial<Scope> = {}): Scope =>
  ({
    stage: 'dev',
    protect: false,
    home: 'aws',
    project: null,
    app: 'boxlite',
    region: 'ap-southeast-1',
    regionSource: 'test',
    roleArn: null,
    roleArnSource: null,
    roleSessionName: null,
    ...overrides,
  }) as Scope

const google = (): { factory: GoogleFactory; calls: { project: string }[] } => {
  const calls: { project: string }[] = []
  return {
    calls,
    factory: async ({ project }) => {
      calls.push({ project })
      return {
        clients: {} as any,
        auth: { async getProjectId() { return project }, async getCredentials() { return {} } },
      }
    },
  }
}

test('the stage decides, and both halves come from the same decision', async () => {
  const onAws = await resolveHome({ scope: scope({ home: 'aws' }) })
  assert.equal(onAws.identity.home, 'aws')
  assert.equal(onAws.backend.home, 'aws')

  const probe = google()
  const onGcp = await resolveHome({
    scope: scope({ home: 'gcp', project: 'boxlite-dev' }),
    google: probe.factory,
  })
  assert.equal(onGcp.identity.home, 'gcp')
  assert.equal(onGcp.backend.home, 'gcp')
})

test('an AWS stage never reaches for Google', async () => {
  // The default factory throws. An AWS repository must not need those packages
  // installed to run a single command.
  const probe = google()
  await resolveHome({ scope: scope({ home: 'aws' }), google: probe.factory })
  assert.deepEqual(probe.calls, [])
})

test('a GCP stage that declares no project is refused before anything is loaded', async () => {
  // Unlike the AWS account, which is read back from the credentials, the project
  // has to be declared: there is no client to ask until one is open.
  const probe = google()
  await assert.rejects(
    () => resolveHome({ scope: scope({ home: 'gcp', project: null }), google: probe.factory }),
    /has no project; a GCP stage declares which project it lives in/,
  )
  assert.deepEqual(probe.calls, [], 'nothing was loaded before the refusal')
})

test('the project the stage declares is the one the clients are opened against', async () => {
  const probe = google()
  await resolveHome({
    scope: scope({ home: 'gcp', project: 'boxlite-prod' }),
    google: probe.factory,
  })
  assert.deepEqual(probe.calls, [{ project: 'boxlite-prod' }])
})

test('with no factory, the default loader builds the real Google clients', async () => {
  // Strict, and about this repository. The three Google packages are installed
  // here, so the only correct outcome is a resolved GCP home, and deleting any
  // of them has to turn this red.
  //
  // Two weaker shapes were tried and are worth not repeating. One accepted
  // either a resolved home or a "which packages are missing" failure, to stay
  // honest about mstage floating to repositories that never adopted GCP — it
  // accepted both branches and so guarded neither. The other asserted the
  // failure message by building that same message in the test body, which
  // crosses no boundary at all. `loadGoogle`'s message is therefore covered by
  // no standing assertion; what is covered is that the default loader is wired
  // and that the packages it needs are present.
  //
  // Constructing a client reaches no network: Application Default Credentials
  // are only read when something asks who it is.
  const resolved = await resolveHome({ scope: scope({ home: 'gcp', project: 'boxlite-dev' }) })
  assert.equal(resolved.identity.home, 'gcp')
  assert.equal(resolved.backend.home, 'gcp')
})

test('a factory that cannot produce clients fails the command rather than the deploy', async () => {
  await assert.rejects(
    () =>
      resolveHome({
        scope: scope({ home: 'gcp', project: 'boxlite-dev' }),
        google: async () => {
          throw new Error('no application default credentials')
        },
      }),
    /no application default credentials/,
  )
})

test('the bucket the state sits in is read from the record, in the project the stage declares', async () => {
  // Called, not merely counted: a wrong bucket is exactly what a `typeof ===
  // "function"` assertion would let through. On GCP the record lives in Secret
  // Manager, and reading it is what proves the client was opened against the
  // declared project.
  const reads: string[] = []
  const clients = {
    secrets: {
      async accessSecretVersion({ name }: { name: string }) {
        reads.push(name)
        return [{ payload: { data: JSON.stringify({ state: 'mstage-state-0123456789abcdef' }) } }]
      },
    },
  }
  const resolved = await resolveHome({
    scope: scope({ home: 'gcp', project: 'boxlite-dev' }),
    google: async () => ({
      clients: clients as any,
      auth: { async getProjectId() { return 'boxlite-dev' }, async getCredentials() { return {} } },
    }),
  })

  assert.equal(await resolved.stateBucket(), 'mstage-state-0123456789abcdef')
  assert.deepEqual(reads, ['projects/boxlite-dev/secrets/mstage-bootstrap/versions/latest'])
})

test('a command that never asks for the bucket never pays for the lookup', async () => {
  // Which is why it is a function rather than a value: every `env` command
  // resolves a home, and most of them have no use for it.
  let reads = 0
  await resolveHome({
    scope: scope({ home: 'gcp', project: 'boxlite-dev' }),
    google: async () => ({
      clients: {
        secrets: {
          async accessSecretVersion() {
            reads += 1
            return [{ payload: { data: '{}' } }]
          },
        },
      } as any,
      auth: { async getProjectId() { return 'boxlite-dev' }, async getCredentials() { return {} } },
    }),
  })
  assert.equal(reads, 0)
})

test('a home nobody implements is refused by name', async () => {
  await assert.rejects(
    () => resolveHome({ scope: scope({ home: 'azure' as any }) }),
    /Unknown home "azure"/,
  )
})
