// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

import assert from 'node:assert/strict'
import test from 'node:test'

import { GcpBootstrapError, attributeCondition, bootstrapGcp, type Run, type RunResult } from './gcp.js'

const GITHUB = {
  issuer: 'https://token.actions.githubusercontent.com',
  owner: 'boxlite-ai',
  ownerId: '123456',
  repository: 'boxlite',
  repositoryId: '789012',
}

/** The calls this file treats as questions rather than changes. */
const QUERIES = [
  'secrets describe',
  'secrets versions access',
  'storage buckets describe',
  'iam workload-identity-pools describe',
  'iam workload-identity-pools providers describe',
  'iam service-accounts describe',
  'artifacts repositories describe',
]

/**
 * A `run` that records what it was asked to do. `existing` decides how the
 * existence questions are answered; every change succeeds, which is what
 * makes a failing change its own test below.
 */
const recorder = ({ existing = false } = {}) => {
  const calls: string[][] = []
  const stdin: string[] = []
  const run: Run = async (command, args, options = {}) => {
    calls.push([command, ...args])
    if (options.stdin !== undefined && options.stdin !== '') stdin.push(options.stdin)
    const asked = args.join(' ')
    if (asked.startsWith('projects describe')) return { code: 0, stdout: '999999999999\n', stderr: '' }
    // A project already bootstrapped names its bucket here, which is what makes
    // the re-run path reachable at all; an absent record is the first run.
    if (asked.startsWith('secrets versions access')) {
      return existing
        ? { code: 0, stdout: JSON.stringify({ state: 'mstage-state-0123456789abcdef' }), stderr: '' }
        : { code: 254, stdout: '', stderr: 'NOT_FOUND' }
    }
    // Nothing enabled yet, so every service in the list is missing.
    if (asked.startsWith('services list')) return { code: 0, stdout: '', stderr: '' }
    if (QUERIES.some((query) => asked.startsWith(query))) {
      return existing ? { code: 0, stdout: '{}', stderr: '' } : { code: 254, stdout: '', stderr: 'NOT_FOUND' }
    }
    return { code: 0, stdout: '', stderr: '' }
  }
  const applied = (...needles: string[]): string[][] =>
    calls.filter((call) => needles.every((needle) => call.join(' ').includes(needle)))
  return { run, calls, stdin, applied }
}

/** One invocation, with the coordinates a real run would read from mstage/mbuild config. */
const invoke = (run: Run) =>
  bootstrapGcp({
    run,
    project: 'boxlite-gcp-dev',
    region: 'asia-southeast1',
    app: 'boxlite',
    stage: 'gcp-dev',
    repository: 'boxlite-app-gcp-dev',
    github: GITHUB,
    log: () => {},
  })

test('a fresh project gets every prerequisite mdeploy and mbuild cannot create for themselves', async () => {
  const gcloud = recorder()
  const result = await invoke(gcloud.run)

  assert.equal(gcloud.applied('services enable').length, 1, 'no APIs were enabled')
  assert.equal(gcloud.applied('storage buckets create').length, 1, 'no state bucket')
  assert.equal(gcloud.applied('storage buckets update', '--versioning').length, 1, 'the bucket is unversioned')
  assert.equal(gcloud.applied('secrets create', 'mstage-bootstrap').length, 1, 'nothing names the bucket')
  assert.equal(
    gcloud.applied('secrets create', 'mstage-passphrase-boxlite-gcp-dev').length,
    1,
    'the store has nothing to be sealed with',
  )
  assert.equal(gcloud.applied('workload-identity-pools create').length, 1, 'no pool for CI to federate into')
  assert.equal(gcloud.applied('providers create-oidc').length, 1, 'no provider trusting GitHub')
  assert.equal(gcloud.applied('service-accounts create', 'boxlite-gcp-dev-deploy').length, 1, 'no deployer')
  assert.equal(gcloud.applied('service-accounts create', 'boxlite-mbuild').length, 1, 'no image publisher')
  assert.equal(gcloud.applied('artifacts repositories create', 'boxlite-app-gcp-dev').length, 1, 'no docker repository')

  assert.equal(result.deployerEmail, 'boxlite-gcp-dev-deploy@boxlite-gcp-dev.iam.gserviceaccount.com')
  assert.equal(result.publisherEmail, 'boxlite-mbuild@boxlite-gcp-dev.iam.gserviceaccount.com')
  assert.match(result.workloadIdentityProvider, /^projects\/999999999999\/.*\/workloadIdentityPools\/boxlite\/providers\/github$/)
})

test('re-running reconciles what is there instead of creating it again', async () => {
  const gcloud = recorder({ existing: true })
  await invoke(gcloud.run)

  for (const created of ['service-accounts create', 'storage buckets create', 'secrets create']) {
    assert.deepEqual(gcloud.applied(created), [], `${created} ran against a project that already has it`)
  }
  assert.deepEqual(gcloud.applied('workload-identity-pools create'), [])
  assert.deepEqual(gcloud.applied('artifacts repositories create'), [])
  // The provider is still reapplied: its attribute condition is the fence, and
  // a rerun with a different repository has to reach GCP somehow.
  assert.equal(gcloud.applied('providers update-oidc').length, 1)
})

test('every call carries the project and never waits on a prompt', async () => {
  const gcloud = recorder()
  await invoke(gcloud.run)
  assert.ok(gcloud.calls.length > 0)
  for (const call of gcloud.calls) {
    assert.equal(call[0], 'gcloud')
    assert.ok(call.includes('boxlite-gcp-dev'), `no project on: ${call.join(' ')}`)
    assert.ok(call.includes('--quiet'), `no --quiet on: ${call.join(' ')}`)
  }
})

test('the deployer may act as the stage environment, and the publisher as main and the stage', async () => {
  const gcloud = recorder()
  await invoke(gcloud.run)

  const deployerGrant = gcloud
    .applied('service-accounts add-iam-policy-binding', 'boxlite-gcp-dev-deploy@')[0]!
    .join(' ')
  assert.match(deployerGrant, /attribute\.environment\/gcp-dev/)

  const publisherGrants = gcloud.applied('service-accounts add-iam-policy-binding', 'boxlite-mbuild@')
  assert.equal(publisherGrants.length, 2, 'the publisher needs both claim shapes')
  assert.ok(publisherGrants.some((call) => call.join(' ').includes('attribute.ref/refs/heads/main')))
  assert.ok(publisherGrants.some((call) => call.join(' ').includes('attribute.environment/gcp-dev')))
})

test('a service account is probed by the name gcloud accepts, not the one it is created with', async () => {
  // `create` takes the bare id and `describe` takes the full email — probing
  // with the id would make every re-run try to create an account already there.
  const gcloud = recorder()
  await invoke(gcloud.run)
  const probes = gcloud.applied('service-accounts describe')
  assert.equal(probes.length, 2, 'the deployer and the publisher are both probed')
  for (const probe of probes) {
    assert.ok(probe.some((argument) => argument.endsWith('.iam.gserviceaccount.com')), `probed with: ${probe.join(' ')}`)
  }
})

test('neither secret reaches the process table', async () => {
  const gcloud = recorder()
  await invoke(gcloud.run)
  for (const call of gcloud.applied('secrets create')) {
    assert.ok(call.includes('--data-file=-'), `a secret was not fed through stdin: ${call.join(' ')}`)
  }
  assert.equal(gcloud.stdin.length, 2, 'expected the bootstrap record and the passphrase')
  const passphrase = gcloud.stdin.find((value) => !value.startsWith('{'))
  assert.ok(passphrase, 'no passphrase was written')
  assert.equal(Buffer.from(passphrase, 'base64').length, 32)
})

test('the state bucket never reaches the log', async () => {
  const gcloud = recorder()
  const lines: string[] = []
  await bootstrapGcp({
    run: gcloud.run,
    project: 'boxlite-gcp-dev',
    region: 'asia-southeast1',
    app: 'boxlite',
    stage: 'gcp-dev',
    repository: 'boxlite-app-gcp-dev',
    github: GITHUB,
    log: (line) => lines.push(line),
  })
  const created = gcloud.applied('storage buckets create')[0]!
  const bucket = created.find((argument) => argument.startsWith('gs://'))!.replace('gs://', '')
  assert.match(bucket, /^mstage-state-[0-9a-f]{16}$/, 'the bucket name is guessable')
  assert.ok(!lines.some((line) => line.includes(bucket)), 'the state bucket name was logged')
})

test('the identity provider pins the repository, and never the audience', () => {
  const condition = attributeCondition(GITHUB)
  assert.match(condition, /repository_owner_id == '123456'/)
  assert.match(condition, /repository_id == '789012'/)
  assert.doesNotMatch(condition, /repository ==/, 'the condition pins a mutable name')
})

test('a project number that cannot be read stops the run before anything is created', async () => {
  const run: Run = async (): Promise<RunResult> => ({ code: 0, stdout: '', stderr: '' })
  await assert.rejects(
    () =>
      bootstrapGcp({
        run,
        project: 'boxlite-gcp-dev',
        region: 'asia-southeast1',
        app: 'boxlite',
        stage: 'gcp-dev',
        repository: 'boxlite-app-gcp-dev',
        github: GITHUB,
        log: () => {},
      }),
    (error: Error) => error instanceof GcpBootstrapError && /Could not read the number of project/.test(error.message),
  )
})

test('a failing gcloud call stops the run and carries the reason', async () => {
  const run: Run = async (_command, args): Promise<RunResult> => {
    if (args.join(' ').startsWith('projects describe')) return { code: 0, stdout: '999999999999', stderr: '' }
    if (args.join(' ').startsWith('services list')) return { code: 0, stdout: '', stderr: '' }
    return { code: 1, stdout: '', stderr: 'PERMISSION_DENIED: caller lacks serviceusage.services.enable' }
  }
  await assert.rejects(
    () => invoke(run),
    (error: Error) => error instanceof GcpBootstrapError && /PERMISSION_DENIED/.test(error.message),
  )
})
