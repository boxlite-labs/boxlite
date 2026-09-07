// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

import assert from 'node:assert/strict'
import test from 'node:test'

import { AwsBootstrapError, bootstrapAws, render, type Run } from './aws.js'

/** The calls this file treats as questions rather than changes. */
const PRESENCE_QUERIES = ['iam get-policy', 'iam get-role', 'ecr describe-repositories', 's3api head-bucket']

/**
 * A `run` that records what it was asked to do. `existing` decides how the
 * existence questions are answered; `versions` seeds what `list-policy-versions`
 * reports. Every change succeeds, which is what makes a failing change its own
 * test below.
 */
const recorder = ({ existing = false, versions = [] as any[] } = {}) => {
  const calls: string[][] = []
  const run: Run = async (command, args) => {
    calls.push([command, ...args])
    const asked = args.join(' ')
    if (asked.startsWith('iam list-policy-versions')) {
      return { code: 0, stdout: JSON.stringify({ Versions: versions }), stderr: '' }
    }
    if (PRESENCE_QUERIES.some((query) => asked.startsWith(query))) {
      return existing ? { code: 0, stdout: '{}', stderr: '' } : { code: 254, stdout: '', stderr: 'not found' }
    }
    return { code: 0, stdout: '', stderr: '' }
  }
  const applied = (...needles: string[]): string[][] => calls.filter((call) => needles.every((needle) => call.join(' ').includes(needle)))
  return { run, calls, applied }
}

const SCOPE = { repo: 'boxlite-ai/boxlite', stage: 'dev', accountId: '123456789012', region: 'ap-southeast-1' }

/** One invocation, with the coordinates a real run would read from bootstrap.ts's main(). */
const invoke = (run: Run) => bootstrapAws({ run, ...SCOPE, log: () => {} })

test('a fresh stage gets every prerequisite mdeploy and mbuild cannot create for themselves', async () => {
  const aws = recorder()
  const result = await invoke(aws.run)

  assert.equal(aws.applied('iam create-policy').length, 1, 'no runtime boundary')
  assert.equal(aws.applied('iam create-role').length, 1, 'no deploy role')
  assert.equal(aws.applied('iam put-role-policy').length, 1, 'no inline policy')
  assert.equal(aws.applied('ecr create-repository').length, 1, 'no Api repository')
  assert.equal(aws.applied('s3api create-bucket').length, 1, 'no artifacts bucket')
  assert.equal(aws.applied('s3api put-bucket-encryption').length, 1)
  assert.equal(aws.applied('s3api put-bucket-versioning').length, 1)
  assert.equal(aws.applied('s3api put-public-access-block').length, 1)
  assert.equal(aws.applied('s3api put-bucket-lifecycle-configuration').length, 1)

  assert.equal(result.roleArn, 'arn:aws:iam::123456789012:role/boxlite-dev-github-deploy')
  assert.equal(result.boundaryArn, 'arn:aws:iam::123456789012:policy/boxlite-dev-runtime-boundary')
  assert.equal(result.repositoryName, 'boxlite-app-dev-api')
  assert.equal(result.bucketName, 'boxlite-app-dev-artifacts-123456789012')

  // The rendered trust document carries no placeholder left to substitute.
  const createRoleCall = aws.applied('iam create-role')[0]
  const trustArgIndex = createRoleCall.indexOf('--assume-role-policy-document') + 1
  assert.doesNotMatch(createRoleCall[trustArgIndex], /<[A-Z]+>/)
})

test('re-running reconciles what is there instead of creating it again', async () => {
  const aws = recorder({ existing: true, versions: [{ VersionId: 'v1', IsDefaultVersion: true, CreateDate: '2026-01-01T00:00:00Z' }] })
  await invoke(aws.run)

  for (const created of ['iam create-policy ', 'iam create-role', 'ecr create-repository', 's3api create-bucket']) {
    assert.deepEqual(aws.applied(created), [], `${created} ran against a stage that already has it`)
  }

  assert.equal(aws.applied('iam create-policy-version').length, 1)
  assert.equal(aws.applied('iam update-assume-role-policy').length, 1)
  assert.equal(aws.applied('iam update-role', '--max-session-duration').length, 1)
  // Reapplied every run regardless of presence, matching the CFN template's idempotent desired state.
  assert.equal(aws.applied('iam put-role-policy').length, 1)
  assert.equal(aws.applied('s3api put-bucket-encryption').length, 1)
})

test('a runtime boundary at the version cap is pruned before a new version is created', async () => {
  const versions = Array.from({ length: 5 }, (_, index) => ({
    VersionId: `v${index + 1}`,
    IsDefaultVersion: index === 4,
    CreateDate: `2026-01-0${index + 1}T00:00:00Z`,
  }))
  const aws = recorder({ existing: true, versions })
  await invoke(aws.run)

  const pruned = aws.applied('iam delete-policy-version')
  assert.equal(pruned.length, 1, 'exactly one old version must be pruned to make room for the new one')
  assert.ok(pruned[0].includes('v1'), 'the oldest non-default version is the one pruned, not an arbitrary one')
  assert.equal(aws.applied('iam create-policy-version').length, 1)
})

test('a runtime boundary well under the version cap is never pruned', async () => {
  const versions = [
    { VersionId: 'v1', IsDefaultVersion: false, CreateDate: '2026-01-01T00:00:00Z' },
    { VersionId: 'v2', IsDefaultVersion: true, CreateDate: '2026-01-02T00:00:00Z' },
  ]
  const aws = recorder({ existing: true, versions })
  await invoke(aws.run)

  assert.deepEqual(aws.applied('iam delete-policy-version'), [])
})

test('render fills in every placeholder and leaves the rest untouched', () => {
  const rendered = render('<REPO> <STAGE> <ACCOUNT> <REGION> unchanged', SCOPE)
  assert.equal(rendered, 'boxlite-ai/boxlite dev 123456789012 ap-southeast-1 unchanged')
})

test('create-bucket omits the location constraint only in us-east-1', async () => {
  const usEast1 = recorder()
  await bootstrapAws({ run: usEast1.run, ...SCOPE, region: 'us-east-1', log: () => {} })
  const [createBucket] = usEast1.applied('s3api create-bucket')
  assert.ok(createBucket, 'no create-bucket call recorded')
  assert.doesNotMatch(createBucket.join(' '), /create-bucket-configuration/)

  const otherRegion = recorder()
  await invoke(otherRegion.run)
  const [createBucketOther] = otherRegion.applied('s3api create-bucket')
  assert.match(createBucketOther.join(' '), /LocationConstraint=ap-southeast-1/)
})

test('a failing AWS call surfaces the CLI stderr rather than a generic failure', async () => {
  const run: Run = async (_command, args) => {
    if (args.join(' ').startsWith('iam get-policy')) return { code: 254, stdout: '', stderr: 'not found' }
    if (args.join(' ').startsWith('iam create-policy')) return { code: 1, stdout: '', stderr: 'AccessDenied: nope' }
    return { code: 0, stdout: '', stderr: '' }
  }
  await assert.rejects(invoke(run), (error: any) => error instanceof AwsBootstrapError && /AccessDenied: nope/.test(error.message))
})
