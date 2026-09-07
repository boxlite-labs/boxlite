import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { fileURLToPath } from 'node:url'

const workflow = readFileSync(
  fileURLToPath(new URL('../../../../.github/workflows/mbuild.yml', import.meta.url)),
  'utf8',
)

/** What the job runs, with the commentary that discusses it removed. */
const commands = workflow
  .split('\n')
  .filter((line) => !/^\s*#/.test(line))
  .join('\n')

// publish() and promote() log in before they build or pull, and a promotion
// logs into both registries — which a step holding only the target stage could
// not do. A login in the workflow as well would write the same token twice.
test('leaves the registry login to mbuild', () => {
  assert.doesNotMatch(commands, /docker login/)
  assert.doesNotMatch(commands, /get-login-password/)
  assert.doesNotMatch(commands, /configure-docker/)
})

// The path has to come from RUNNER_TEMP, the default runner variable, and not
// from the `runner` context: GitHub offers that context to a step but not to
// an `env` block, where it is rejected as an unrecognized named-value.
test('takes the credential path from the runner variable, not the runner context', () => {
  assert.match(commands, /echo "DOCKER_CONFIG=\$RUNNER_TEMP\/docker" >> "\$GITHUB_ENV"/)
  assert.doesNotMatch(commands, /DOCKER_CONFIG.*\$\{\{\s*runner\./)
})

// GITHUB_ENV reaches the steps after the one that writes it, so the order is
// the whole point: a login in an earlier step would still land in HOME.
test('redirects the credentials before mbuild logs in', () => {
  const redirected = commands.indexOf('DOCKER_CONFIG=$RUNNER_TEMP/docker')
  const publishes = commands.indexOf('mbuild publish')
  const promotes = commands.indexOf('mbuild promote')

  assert.notEqual(redirected, -1, 'the job must say where docker writes its credentials')
  assert.ok(redirected < publishes, 'a publish must log in with the credentials already redirected')
  assert.ok(redirected < promotes, 'so must a promotion')
})

/*
 * The retry loop skips the one failure that is not transient.
 *
 * `mbuild publish` exits 78 when the image scan refuses a commit, and that
 * answer does not change on a second ask — it is about the image's own
 * contents. Retrying it spends three attempts and ninety seconds of backoff
 * re-reading the same findings before reporting them.
 *
 * Asserted against the workflow rather than against a constant, because the
 * shell is where the decision is made and a constant would only agree with
 * itself.
 */
test('a scan refusal stops the retry loop rather than being retried', () => {
  const recognises = commands.indexOf('-eq 78')
  const sleeps = commands.indexOf('sleep $((attempt * 15))')

  assert.notEqual(recognises, -1, 'the publish step has to recognise the scan gate’s own exit code')
  assert.notEqual(sleeps, -1, 'and it still has to back off for the failures that are transient')
  assert.ok(recognises < sleeps, 'the refusal has to be recognised before the backoff, or it is retried anyway')
})
