import assert from 'node:assert/strict'
import test from 'node:test'
import { CoordinatesError, assertPromotable, coordinatesOf } from '../src/coordinates.ts'

const never = async (): Promise<string> => {
  throw new Error('the account must not be asked for')
}

test('ECR is addressed by the account the credentials in hand belong to', async () => {
  // Not by the config: an ECR address derived from a declared account could name
  // a repository the push is not authorised in, and the push would be the thing
  // that failed rather than the address.
  let asked = 0
  const resolved = await coordinatesOf({
    stage: 'prod',
    kind: 'ecr',
    project: 'a-project-that-must-be-ignored',
    accountId: async () => {
      asked += 1
      return '000000000000'
    },
  })
  assert.deepEqual(resolved, { accountId: '000000000000' })
  assert.equal(asked, 1)
})

test('Artifact Registry is addressed by the project the stage declares', async () => {
  // And the account is never asked for, because nothing here holds a Google
  // identity to ask it of.
  const resolved = await coordinatesOf({
    stage: 'dev',
    kind: 'artifact-registry',
    project: 'example-project-0000',
    accountId: never,
  })
  assert.deepEqual(resolved, { project: 'example-project-0000' })
})

test('a stage that publishes to Artifact Registry and declares no project is refused', async () => {
  await assert.rejects(
    () => coordinatesOf({ stage: 'dev', kind: 'artifact-registry', project: null, accountId: never }),
    (error) => error instanceof CoordinatesError && /gives stage "dev" no project/.test(error.message),
  )
})

test('a promotion within one registry kind is allowed', () => {
  assert.doesNotThrow(() =>
    assertPromotable({ from: { stage: 'dev', kind: 'ecr' }, to: { stage: 'prod', kind: 'ecr' } }),
  )
})

test('a promotion across clouds is refused rather than half-attempted', () => {
  // One identity cannot hold both clouds, so the pull would authenticate and
  // the push would not — partway through a set of artifacts.
  assert.throws(
    () =>
      assertPromotable({
        from: { stage: 'dev', kind: 'artifact-registry' },
        to: { stage: 'prod', kind: 'ecr' },
      }),
    /Cannot promote dev \(artifact-registry\) to prod \(ecr\)/,
  )
})
