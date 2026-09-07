/*
 * The deploy workflow, held against what mdeploy actually does.
 *
 * A workflow is the one part of a deploy nothing else typechecks, and the
 * mistakes it makes are the expensive kind: a gate that runs after the thing it
 * was meant to refuse, a retry around an answer that will not change, a cloud
 * named in a place that has to work on both.
 */

import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { fileURLToPath } from 'node:url'

const workflow = readFileSync(fileURLToPath(new URL('../../../../.github/workflows/mdeploy.yml', import.meta.url)), 'utf8')

/** What the workflow runs, with the commentary that discusses it removed. */
const commands = workflow
  .split('\n')
  .filter((line) => !/^\s*#/.test(line))
  .join('\n')

test('the cloud is read from the config rather than written into the workflow', () => {
  // Adding a stage on another cloud has to be an edit to mstage.config.json. A
  // registry host, a repository name or a region written here would be a second
  // declaration, and the one that keeps pointing at the old cloud.
  assert.match(commands, /mbuild inspect -- --stage/)
  assert.doesNotMatch(commands, /dkr\.ecr\./)
  assert.doesNotMatch(commands, /docker\.pkg\.dev/)
  assert.doesNotMatch(commands, /ap-southeast-1|asia-southeast1/)
})

test('both clouds can be federated, and each only when it is the one', () => {
  assert.match(workflow, /aws-actions\/configure-aws-credentials/)
  assert.match(workflow, /google-github-actions\/auth/)
  assert.match(workflow, /if: steps\.config\.outputs\.kind == 'ecr'/)
  assert.match(workflow, /if: steps\.config\.outputs\.kind == 'artifact-registry'/)
})

/*
 * The AWS role is composed, not looked up in a repository variable.
 *
 * Every other deploy workflow here builds it from `vars.AWS_ACCOUNT_ID` and the
 * fixed per-stage role name. A workflow that read `vars.AWS_DEPLOY_ROLE_ARN`
 * instead would federate against a variable this repository does not define —
 * the OIDC step fails with an empty role, which reads as an auth problem rather
 * than as a missing setting.
 *
 * Both new workflows are checked here so the two cannot drift apart.
 */
test('the AWS role is composed from the account id, as every other workflow does', () => {
  const mbuild = readFileSync(fileURLToPath(new URL('../../../../.github/workflows/mbuild.yml', import.meta.url)), 'utf8')
  for (const [name, source] of [
    ['mdeploy.yml', workflow],
    ['mbuild.yml', mbuild],
  ] as const) {
    assert.match(
      source,
      /role-to-assume: arn:aws:iam::\$\{\{ vars\.AWS_ACCOUNT_ID \}\}:role\/boxlite-\$\{\{ [^}]+ \}\}-github-deploy/,
      `${name} does not compose the role ARN`,
    )
    assert.doesNotMatch(source, /vars\.AWS_DEPLOY_ROLE_ARN|vars\.AWS_ECR_PUSH_ROLE_ARN/, `${name} reads an undefined variable`)
  }
})

test('every gate runs before the apply, because a refusal afterwards is not a gate', () => {
  const digest = commands.indexOf('mstage env digest')
  const images = commands.indexOf('mbuild verify')
  const apply = commands.indexOf('npm run mdeploy -- --stage')
  for (const [name, index] of [
    ['the digest check', digest],
    ['the image check', images],
  ] as const) {
    assert.notEqual(index, -1, `${name} is missing`)
    assert.ok(index < apply, `${name} runs after the apply`)
  }
})

test('the image check comes before anything that changes a shared resource', () => {
  // A deploy dispatched while its commit is still being published used to fail
  // minutes in, on a task that could not pull, after the apply had already
  // created resources.
  assert.ok(commands.indexOf('mbuild verify') < commands.indexOf('npm run mdeploy -- --stage'))
})

test('a read is retried and the apply is not', () => {
  // A run killed mid-apply can leave the per-stage lock held, and the second
  // attempt then fails on the lock rather than on the cause worth reading.
  const session = commands.slice(commands.indexOf('Verify the session'), commands.indexOf('Verify the stage'))
  assert.match(session, /for attempt in 1 2 3/)

  const apply = commands.slice(commands.indexOf('- name: Apply'))
  assert.doesNotMatch(apply, /for attempt in/)
  assert.doesNotMatch(apply, /sst unlock|state unlock -- --stage "\$\{\{/, 'nothing clears the lock automatically')
})

test('the digest and the image check are not retried, because their answer is not transient', () => {
  const between = commands.slice(commands.indexOf('Verify the stage configuration'), commands.indexOf('- name: Confirm'))
  assert.doesNotMatch(between, /for attempt in/)
})

test('a preview is the default and an apply has to be asked for', () => {
  assert.match(workflow, /apply:\n\s+description:[^\n]*\n\s+required: true\n\s+type: boolean\n\s+default: false/)
  assert.match(commands, /--diff/)
})

test('a protected stage is confirmed, and the confirmation reaches mdeploy', () => {
  assert.match(commands, /is protected in mstage\.config\.json/)
  assert.match(commands, /inputs\.confirm && '--confirm'/)
})

test('both mdeploy dispatches share one concurrency group, which the state requires', () => {
  // An app and stage keep one checkpoint. Two applies against one stage read
  // and write the same file, and the second to finish erases the first.
  assert.match(workflow, /group: mdeploy-\$\{\{ inputs\.stage \}\}/)
  assert.match(workflow, /cancel-in-progress: false/)
})
