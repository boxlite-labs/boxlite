// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import test from 'node:test'
import { load as loadYaml } from 'js-yaml'

import { DEFAULT_AWS_REGION } from './environment.js'
import { verifyDeployRoleGrantsBoundaryPermission } from './role-boundary.js'
import { githubDeployRoleName } from '../bootstrap/environment.js'
import { render } from '../bootstrap/aws.js'
import { liveText } from '../shared/live-source.js'
import { apiImageRepository } from '../artifacts/api.js'
import { runnerArtifactsBucketName } from '../artifacts/runner.js'

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..')
const SST_WRAPPER = join(REPO_ROOT, 'apps/infra/deployment/sst.ts')
const RUNNER_POLICY_RUNTIME_PROJECT = join(REPO_ROOT, 'apps/infra/policies/runner/PulumiPolicy.yaml')
const RUNNER_POLICY_ENTRY = join(REPO_ROOT, 'apps/infra/policies/runner/index.ts')
const RUNNER_POLICY_DEFINITIONS = join(REPO_ROOT, 'apps/infra/policies/runner/definitions.ts')
const DEPLOY_WORKFLOW = join(REPO_ROOT, '.github/workflows/deploy-infra.yml')
const RELEASE_DEPLOY_WORKFLOW = join(REPO_ROOT, '.github/workflows/deploy-release.yml')
const API_IMAGE_BUILD_WORKFLOW = join(REPO_ROOT, '.github/workflows/build-apps-api-image.yml')
const BUILD_C_WORKFLOW = join(REPO_ROOT, '.github/workflows/build-c.yml')
const BUILD_RUNNER_WORKFLOW = join(REPO_ROOT, '.github/workflows/build-runner-binary.yml')
const E2E_CLOUD_WORKFLOW = join(REPO_ROOT, '.github/workflows/e2e-cloud.yml')
const LINT_WORKFLOW = join(REPO_ROOT, '.github/workflows/lint.yml')
const DEV_DEPLOY_ROLE_TRUST = join(REPO_ROOT, 'apps/infra/bootstrap/aws/deploy-role-trust.json')
const DEV_DEPLOY_ROLE_POLICY = join(REPO_ROOT, 'apps/infra/bootstrap/aws/deploy-role-policy.json')
const DEV_RUNTIME_BOUNDARY_POLICY = join(REPO_ROOT, 'apps/infra/bootstrap/aws/runtime-boundary-policy.json')
const load = (source: string): any => loadYaml(source)
const values = (record: any): any[] => Object.values(record ?? {})
const entries = (record: any): Array<[string, any]> => Object.entries(record ?? {})

// One decision per artifact kind, made where the text is read (see shared/live-source.ts).
const liveShell = (run: string) => liveText('shell', run)
const assertShellLine = (run: string, pattern: RegExp, message?: string) =>
  assert.match(liveShell(run), pattern, message ?? `missing live shell: ${pattern}`)
const assertLiveLine = (text: string, pattern: RegExp, message?: string) =>
  assert.match(liveText('script', text), pattern, message ?? `missing live line: ${pattern}`)

/*
 * /sst/bootstrap names the state and asset buckets every stage shares, and SST reads it before it
 * knows its stage, so it cannot be stage-scoped — which makes it the one parameter a stage must never
 * be able to write. Rewriting it repoints every other stage's state; deleting it breaks them all.
 *
 * Deciding which statements reach it is the whole difficulty, so it is a function with its own tests
 * rather than a condition inline in one assertion. Reading the template alone cannot prove the logic is
 * right — the template passes, and a matcher that answered "nothing matches" would pass just as well.
 */
const SHARED_BOOTSTRAP_PARAMETER = '/sst/bootstrap'

/*
 * IAM matches resources by glob, so this must too: `parameter/sst/*`, `parameter/*`, `*`, and
 * `parameter/sst/boot?trap` all reach the shared bootstrap while equalling no literal. Both wildcards
 * IAM supports are translated and every other metacharacter escaped, so a `.` in an ARN cannot quietly
 * stand in for an arbitrary character.
 */
function iamGlob(pattern: string) {
  const escaped = pattern.replace(/[.+^${}()|[\]\\]/g, '\\$&')
  return new RegExp(
    `^${escaped.replace(/\*/g, '.*').replace(/\?/g, '.').split(SUB_SENTINEL).join('.*')}$`,
    'i',
  )
}

/*
 * Segment the ARN rather than looking for a `:parameter` substring. `arn:aws:ssm:*:*:*` covers the
 * shared bootstrap and contains no such marker, so a substring search silently answers "no" to the
 * broadest grant there is.
 *
 * A `${...}`-style computed value (a CloudFormation intrinsic, or anything templated the same way)
 * has to be neutralised first if it could contain a colon of its own and split into phantom
 * segments — deploy-role-policy.json's own placeholders (`<REGION>`, `<ACCOUNT>`, `<STAGE>`) carry
 * none, so they never need this, but sharedBootstrapMutations still has to refuse anything that
 * does rather than guess what it stands for.
 */
const SUB_SENTINEL = '\u0001'

function coversSharedBootstrap(resource: unknown) {
  if (typeof resource !== 'string') return false
  if (resource === '*') return true

  const segments = resource.replace(/\$\{[^}]*\}/g, SUB_SENTINEL).split(':')
  // Anything that is not a well-formed 6-field ARN. Whether IAM rejects such a resource or reads the
  // omitted fields as wildcards is not something this can settle, and it does not need to: either way
  // "not an ARN I can read" must not come back as "does not reach the shared bootstrap".
  if (segments.length < 6 || segments[0] !== 'arn') return true
  const service = segments[2]
  if (service !== 'ssm' && service !== '*' && service !== SUB_SENTINEL) return false
  // Rejoined: an SSM parameter name may itself contain colons.
  const resourceId = segments.slice(5).join(':')
  return iamGlob(resourceId).test(`parameter${SHARED_BOOTSTRAP_PARAMETER}`)
}

/*
 * Whether an action can write a parameter — case-insensitively, because IAM action names are:
 * `SSM:PutParameter` and `ssm:putparameter` are the same grant, so a case-sensitive prefix test is an
 * evasion rather than a style question. `*` and `ssm:*` grant every write without naming one.
 */
function mutatesParameters(action: unknown) {
  if (typeof action !== 'string') return false
  // A substitution can resolve to anything, so `${Service}:PutParameter` has to read as a possible
  // write rather than as "not an ssm action". Same direction the resource side widens in: unknown
  // means flagged, never waved through.
  if (action.includes('${')) return true
  const normalized = action.toLowerCase()
  if (normalized === '*' || normalized === 'ssm:*') return true
  return normalized.startsWith('ssm:') && !/^ssm:(get|list|describe)/.test(normalized)
}

/*
 * An Allow with NotResource grants everything EXCEPT what it lists, so it reaches /sst/bootstrap unless
 * that is one of the exclusions; NotAction is the same inversion for actions. Reasoning about either
 * properly means evaluating a complement, which this does not do — so rather than inspect Action and
 * Resource and quietly return "nothing matches", it reports the statement as unanalyzable and the
 * assertion fails. The template uses neither today, and this is what keeps that true.
 */
function sharedBootstrapMutations(statement: any) {
  const asList = (value: any) => (Array.isArray(value) ? value : [value])

  /*
   * Only a grant can be a violation. A Deny naming the shared bootstrap is the opposite — a protection
   * — and flagging it would make the guard reject the very thing it wants. Anything that is neither is
   * refused rather than assumed harmless, since IAM requires Effect and a statement without one is
   * malformed.
   */
  const effect = statement?.Effect
  if (effect === 'Deny') return []
  if (effect !== 'Allow') return [`unanalyzable: Effect ${JSON.stringify(effect)}`]

  const inverted = ['NotAction', 'NotResource'].filter((key) => statement?.[key] !== undefined)
  if (inverted.length > 0) return [`unanalyzable: ${inverted.join(' + ')}`]

  /*
   * Anything that is not a plain string. `!Sub` with a variable map, or `Fn::Join`, parses to an array
   * or object rather than to text, and the value it computes cannot be read here — so it is refused
   * rather than skipped. Skipping is the failure mode that matters: a computed resource would return
   * "does not cover the shared bootstrap" while possibly naming exactly that.
   */
  const resources = asList(statement?.Resource)
  const actions = asList(statement?.Action)
  const computed = [...resources, ...actions].filter((entry) => typeof entry !== 'string')
  if (computed.length > 0) return [`unanalyzable: ${computed.length} computed Action/Resource entrie(s)`]

  if (!resources.some(coversSharedBootstrap)) return []
  return actions.filter(mutatesParameters)
}

test('the shared-bootstrap guard catches every way a grant can cover it', () => {
  // Synthetic statements, not the template: this is what can make the guard above fail. Each entry
  // reaches /sst/bootstrap without naming it exactly, or spells a write so a naive prefix test misses
  // it.
  const evasions = [
    { Effect: 'Allow', Sid: 'ExactArn', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    { Effect: 'Allow', Sid: 'TrailingStar', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/*' },
    { Effect: 'Allow', Sid: 'EmbeddedStar', Action: 'ssm:DeleteParameter', Resource: 'arn:aws:ssm:r:a:parameter/*/bootstrap' },
    { Effect: 'Allow', Sid: 'SingleCharWildcard', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/boot?trap' },
    { Effect: 'Allow', Sid: 'EverythingResource', Action: 'ssm:PutParameter', Resource: '*' },
    // No `:parameter` anywhere in it, yet it covers every SSM resource in the account.
    { Effect: 'Allow', Sid: 'WildcardArnSegments', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:*:*:*' },
    { Effect: 'Allow', Sid: 'WildcardServiceSegment', Action: 'ssm:PutParameter', Resource: 'arn:aws:*:*:*:parameter/sst/bootstrap' },
    { Effect: 'Allow', Sid: 'ServiceWildcardAction', Action: 'ssm:*', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    { Effect: 'Allow', Sid: 'EveryAction', Action: '*', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    { Effect: 'Allow', Sid: 'UppercaseService', Action: 'SSM:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    { Effect: 'Allow', Sid: 'MixedCaseAction', Action: 'SsM:putParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    {
      Effect: 'Allow',
      Sid: 'ListedAmongReads',
      Action: ['ssm:GetParameter', 'ssm:PutParameter'],
      Resource: ['arn:aws:ssm:r:a:parameter/sst/*'],
    },
    // Inverted forms: an Allow that lists what it does NOT cover grants the rest, including this.
    { Effect: 'Allow', Sid: 'NotResourceAllow', Action: 'ssm:PutParameter', NotResource: 'arn:aws:ssm:r:a:parameter/other' },
    { Effect: 'Allow', Sid: 'NotActionAllow', NotAction: 's3:*', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    // A computed identifier: nothing here can prove ${Whatever} is not the shared bootstrap.
    { Effect: 'Allow', Sid: 'SubstitutedIdentifier', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/${Whatever}' },
    // Not text at all. `!Sub` with a variable map and `Fn::Join` survive YAML parsing as structures,
    // and their result is not knowable here, so neither may be waved through.
    {
      Effect: 'Allow',
      Sid: 'FnJoinResource',
      Action: 'ssm:PutParameter',
      Resource: { 'Fn::Join': [':', ['arn', 'aws', 'ssm', 'r', 'a', 'parameter/sst/bootstrap']] },
    },
    {
      Effect: 'Allow',
      Sid: 'FnSubWithVariableMap',
      Action: 'ssm:PutParameter',
      Resource: ['arn:aws:ssm:r:a:parameter/${Name}', { Name: 'sst/bootstrap' }],
    },
    { Effect: 'Allow', Sid: 'ComputedAction', Action: { 'Fn::Join': [':', ['ssm', 'PutParameter']] }, Resource: '*' },
    // Malformed rather than permissive, but still not something this can reason about.
    { Sid: 'NoEffect', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    // Not a readable 6-field ARN, so it must never come back as "reaches nothing".
    { Effect: 'Allow', Sid: 'TruncatedArn', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm' },
    // A substituted action: the service is only known at deploy time, so it could be ssm.
    {
      Effect: 'Allow',
      Sid: 'SubstitutedActionService',
      Action: '${Service}:PutParameter',
      Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap',
    },
    {
      Effect: 'Allow',
      Sid: 'SubstitutedActionVerb',
      Action: 'ssm:${Verb}',
      Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap',
    },
    { Effect: 'Allow', Sid: 'NotAnArn', Action: 'ssm:PutParameter', Resource: 'parameter/sst/bootstrap' },
  ]
  for (const statement of evasions) {
    assert.notDeepEqual(sharedBootstrapMutations(statement), [], `${statement.Sid} must be caught`)
  }

  // And what must NOT trip it, or the guard would reject the policy the stack actually needs.
  const allowed = [
    {
      Effect: 'Allow',
      Sid: 'ReadOnlyShared',
      Action: ['ssm:GetParameter', 'ssm:GetParameters'],
      Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap',
    },
    { Effect: 'Allow', Sid: 'DescribeEverywhere', Action: 'ssm:DescribeParameters', Resource: '*' },
    { Effect: 'Allow', Sid: 'ThisStagesParameters', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/boxlite/dev/*' },
    { Effect: 'Allow', Sid: 'PassphraseOnly', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/passphrase/boxlite/dev' },
    { Effect: 'Allow', Sid: 'DifferentService', Action: 's3:PutObject', Resource: '*' },
    // A wildcard ARN for a different service reaches no parameter at all.
    { Effect: 'Allow', Sid: 'WildcardArnOtherService', Action: 'ssm:PutParameter', Resource: 'arn:aws:s3:*:*:*' },
    // A Deny naming the shared bootstrap protects it. Flagging that would reject the fix, not the bug.
    { Effect: 'Deny', Sid: 'DenyProtectsIt', Action: 'ssm:PutParameter', Resource: 'arn:aws:ssm:r:a:parameter/sst/bootstrap' },
    { Effect: 'Deny', Sid: 'DenyInverted', NotAction: 's3:*', Resource: '*' },
  ]
  for (const statement of allowed) {
    assert.deepEqual(sharedBootstrapMutations(statement), [], `${statement.Sid} must be allowed`)
  }
})

/*
 * The stage's configuration reaches a deploy through the SST secret store, which
 * deployment/sst.ts reads with the AWS credentials the job already holds. Nothing may reintroduce
 * the GitHub half of that: `DEPLOY_ENV` put stage config in a second control plane, and writing it
 * out as apps/infra/.env made a gitignored local file a CI transport format.
 *
 * Asserted as an absence, over the whole file rather than one step, because the failure this guards
 * is a step being *added* back — anywhere in the job.
 */
function assertNoMaterializedStageConfig(workflow: any, source: string) {
  assert.doesNotMatch(source, /secrets\.DEPLOY_ENV/, 'stage config must come from the SST secret store, not DEPLOY_ENV')
  // The redirect specifically, not any mention of the path: the comment above the capability gate
  // names apps/infra/.env to explain what an old commit expects, and a guard that cannot tell prose
  // from a write would forbid explaining the thing it protects.
  assert.doesNotMatch(source, />\s*apps\/infra\/\.env/, 'no workflow step may write apps/infra/.env')
  assert.doesNotMatch(source, /rm -f apps\/infra\/\.env/, 'nothing should need to clean up a materialized .env')
  assert.doesNotMatch(source, /validate-environment/, 'the DEPLOY_ENV validator went away with DEPLOY_ENV')
  for (const step of workflow.jobs.deploy.steps) {
    assert.notEqual(step.name, 'Materialize stage configuration')
    assert.notEqual(step.name, 'Remove materialized configuration')
  }
}

/*
 * The deploy role ARN is read before any AWS credentials exist, so it cannot come from the store.
 * Only the account id is unknown though — the role name is githubDeployRoleName(stage) — so the
 * workflows compose it from the stage's AWS_ACCOUNT_ID instead of a per-stage AWS_DEPLOY_ROLE_ARN.
 * Pinned against the same helper bootstrap uses, so the two cannot drift.
 */
function assertComposedDeployRoleArn(source: string, stageExpression: string) {
  const expected = `role-to-assume: arn:aws:iam::\${{ vars.AWS_ACCOUNT_ID }}:role/${githubDeployRoleName('STAGE').replace('STAGE', stageExpression)}`
  assert.ok(source.includes(expected), `missing composed deploy role ARN: ${expected}`)
  assert.doesNotMatch(source, /vars\.AWS_DEPLOY_ROLE_ARN/, 'the per-stage role ARN variable is gone')
  // The region keeps its optional per-stage override — a stage in another region has no other way to
  // say so, since it is read before any AWS access exists and so cannot come from the secret store.
  // What matters is that the DEFAULT is the wrapper's own constant, so a stage that configures nothing
  // deploys where every local command resolves to.
  assert.ok(
    source.includes(`\${{ vars.AWS_REGION || '${DEFAULT_AWS_REGION}' }}`),
    `missing the defaulted region expression for ${DEFAULT_AWS_REGION}`,
  )
}

function readDeployRolePolicy() {
  return JSON.parse(readFileSync(DEV_DEPLOY_ROLE_POLICY, 'utf8'))
}

function readDeployRoleTrust() {
  return JSON.parse(readFileSync(DEV_DEPLOY_ROLE_TRUST, 'utf8'))
}

function readRuntimeBoundaryStatements() {
  return JSON.parse(readFileSync(DEV_RUNTIME_BOUNDARY_POLICY, 'utf8')).Statement
}

/*
 * Every statement that reaches the deploy role. bootstrap/aws.ts's ensureDeployRole is the only
 * place in this repository that grants anything to boxlite-<stage>-github-deploy — one
 * `put-role-policy` call against this one file, no managed policy ever attached — so unlike a
 * CloudFormation template, there is no second resource type a grant could hide in.
 */
function deployRolePolicyStatements() {
  return readDeployRolePolicy().Statement
}

function findStatement(statements: any, sid: any) {
  const statement = statements.find((candidate: any) => candidate.Sid === sid)
  assert.ok(statement, `missing ${sid} statement`)
  return statement
}

test('SST deploy verifies the selected Runner artifact before invoking SST', () => {
  const source = readFileSync(SST_WRAPPER, 'utf8')
  const preflightIndex = source.indexOf('await verifyRunnerArtifact(')
  const sstIndex = source.indexOf('await withPulumiEventLogCleanup(')

  assert.match(source, /requireCheckoutMatchesArtifactRefs, resolveArtifactSource \} from '\.\.\/artifacts\/source\.js'/)
  // The import is not the behavior: commenting the call out leaves the import, and a build deploy
  // would then ship the Proxy and the OtelCollector from this checkout while the Api and the
  // Runner are addressed by a ref that names a different commit.
  //
  // Both sources, not one: a Runner-only build addresses no Api ref, so passing `apiSource` alone
  // would skip the check for exactly the deploy `npm run runner:build-artifact` produces.
  assertLiveLine(source, /requireCheckoutMatchesArtifactRefs\(\[apiSource, runnerSource\]\)/)
  assert.match(source, /verifyRunnerArtifact \} from '\.\.\/artifacts\/runner\.js'/)
  // Resolved only when the scope covers it. Dropping the guard restores the failure this scope
  // exists to avoid: an Api-only deploy demanding a published Runner artifact for a commit whose
  // Runner was deliberately never built, so a complete deploy fails on a missing thing nobody
  // asked for. Pin the conditional itself — `resolveArtifactSource('runner')` alone would pass
  // while verifying a component the plan excludes.
  assertLiveLine(source, /const runnerSource = deployScope\.components\.includes\('runner'\)/)
  assertLiveLine(source, /const apiSource = deployScope\.components\.includes\('api'\)/)
  assert.notEqual(preflightIndex, -1, 'the Runner artifact preflight is missing')
  assert.notEqual(sstIndex, -1, 'the guarded SST invocation is missing')
  assert.ok(preflightIndex < sstIndex, 'SST may run before Runner artifact availability is known')
  // The scope comes from the args SST is actually handed, resolved once. A second, independent
  // notion of scope here (an env var, a re-parse) could disagree with the plan and verify the
  // wrong half.
  assertLiveLine(source, /deployScope = resolveDeployScope\(sstArgs\)/)
  // Exported before sst is spawned, so the resource graph is built for the same scope as the
  // plan. Without it stack/runners.ts declares UpgradeRunnerBinary-* on an Api-only deploy, and that
  // command — a sibling of the excluded instance, so `--exclude Runner` misses it — installs a
  // Runner binary from a commit whose build-runner job never ran.
  assertLiveLine(source, /exportDeployScope\(deployScope\)/)
  const exportIndex = liveText('script', source).indexOf('exportDeployScope(deployScope)')
  assert.ok(exportIndex !== -1 && exportIndex < sstIndex, 'the scope must be exported before SST is invoked')
  assert.match(source, /withRequiredRunnerPolicy\(sstArgs\)/)
  assert.doesNotMatch(source, /RUNNER_POLICY_ROOT/)
})

test('preview and deploy use the mandatory local Runner policy', () => {
  assert.ok(existsSync(RUNNER_POLICY_ENTRY), 'the Runner policy entry point is missing')
  assert.ok(existsSync(RUNNER_POLICY_DEFINITIONS), 'the Runner policy definitions are missing')

  const policySource = readFileSync(RUNNER_POLICY_ENTRY, 'utf8')
  const policyDefinitions = readFileSync(RUNNER_POLICY_DEFINITIONS, 'utf8')
  assert.match(policySource, /new PolicyPack\('boxlite-runner-safety'/)
  assert.match(policySource, /serializedRunnerStateBaseline = process\.env\.BOXLITE_RUNNER_STATE_BASELINE/)
  assert.match(policySource, /parseRunnerStateBaseline\(serializedRunnerStateBaseline\)/)
  assert.match(policySource, /policies: createRunnerPolicies\(runnerInventory, runnerStateBaseline\)/)
  assert.equal(policyDefinitions.match(/enforcementLevel: 'mandatory'/g)?.length, 2)

  const packageJson = JSON.parse(readFileSync(join(REPO_ROOT, 'apps/infra/package.json'), 'utf8'))
  const packageLock = JSON.parse(readFileSync(join(REPO_ROOT, 'apps/infra/package-lock.json'), 'utf8'))
  assert.equal(packageJson.main, undefined)
  assert.deepEqual(load(readFileSync(RUNNER_POLICY_RUNTIME_PROJECT, 'utf8')), {
    runtime: 'nodejs',
    main: 'index.ts',
    description: 'Mandatory BoxLite Runner lifecycle and identity policy',
  })
  assert.equal(packageJson.devDependencies.typescript, '5.8.3')
  assert.equal(packageJson.devDependencies['ts-node'], '10.9.1')
  assert.equal(packageJson.devDependencies['@pulumi/policy'], '1.21.0')
  assert.equal(packageLock.packages[''].devDependencies['@pulumi/policy'], '1.21.0')
})

test('SST deploy verifies the selected API image before invoking SST', () => {
  // Over live source: unlike the Runner preflight, nothing executes this path in a test, so a
  // commented-out call would otherwise leave both indices and the ordering intact.
  const source = liveText('script', readFileSync(SST_WRAPPER, 'utf8'))
  const preflightIndex = source.indexOf('const image = verifyApiImage(')
  const sstIndex = source.indexOf('await withPulumiEventLogCleanup(')

  assert.match(source, /resolveArtifactSource\('api'\)/)
  assert.notEqual(preflightIndex, -1, 'the API image preflight is missing')
  assert.ok(preflightIndex < sstIndex, 'SST may run before the selected API image is known to exist')
  // Both published sources go through it. Gating on release alone would let a build deploy name a
  // commit tag nothing ever pushed, and discover it when the ECS task fails to pull.
  //
  // The `apiSource &&` is the out-of-scope case, not defensive noise: a Runner-only deploy
  // excludes the Api, leaves apiSource undefined, and would otherwise throw reading `.kind`
  // before SST is reached.
  assertLiveLine(source, /if \(apiSource && \(apiSource\.kind === 'release' \|\| apiSource\.ref\)\) \{/)
})

test('a failed sst call names its log rather than quoting it', () => {
  /*
   * sst writes provider diagnostics to .sst/log/sst.log, in most detail exactly when a call fails —
   * request bodies included. bootstrap hands sst an app secret (`secret set`) and a stage's entire
   * configuration (`secret load`), so quoting that file into an error message would put those values
   * in the operator's terminal and whatever collects it.
   *
   * Asserted as "nothing reads the log" rather than "the calls that carry secrets do not": the log
   * persists across calls, so a later install failure would quote what an earlier secret load wrote,
   * and a per-call rule cannot see that. Removing the reader is what makes the property hold.
   */
  const source = liveText('script', readFileSync(join(REPO_ROOT, 'apps/infra/bootstrap/bootstrap.ts'), 'utf8'))

  /*
   * No read of any kind inside runSst, rather than "no read whose argument mentions sst.log" — a path
   * held in a variable, or a helper called from the catch, would satisfy the narrower rule while
   * quoting the same file. What has to hold is that the failure path reads nothing at all.
   */
  const start = source.indexOf('function runSst(')
  assert.notEqual(start, -1, 'runSst is missing')
  const body = source.slice(start, source.indexOf('\n}\n', start))
  assert.doesNotMatch(body, /readFile|createReadStream|execFileSync\(\s*['"]cat/, 'the failure path must read nothing')

  assert.doesNotMatch(source, /sstLogTail/, 'the log-quoting helper must stay gone')
  // The log is named exactly once, as a path. A second mention is a second thing doing something
  // with it, which is what this is here to notice.
  assert.equal(source.match(/sst\.log/g)?.length, 1, 'the sst log may be named once, as a path')
  assertLiveLine(source, /see \$\{join\(INFRA_ROOT, '\.sst', 'log', 'sst\.log'\)\}/)
})

test('what bootstrap stores is the .env it validated, read once', () => {
  // Over live source: nothing executes bootstrap in a test. main() validates a snapshot before any
  // external mutation, then ensureStageConfig runs after the OIDC provider, the GitHub Environment and
  // possibly an Auth0 app exist — minutes later. A second read there would store whatever the file says
  // by then, which is not what was checked.
  const source = liveText('script', readFileSync(join(REPO_ROOT, 'apps/infra/bootstrap/bootstrap.ts'), 'utf8'))
  const start = source.indexOf('function ensureStageConfig(')
  assert.notEqual(start, -1, 'ensureStageConfig is missing')
  const body = source.slice(start, source.indexOf('\n}\n', start))

  assert.doesNotMatch(body, /readFileSync|requireStageEnvFile/, 'the payload must be handed in, not re-read')
  assert.doesNotMatch(body, /prepareStageConfigLoad/, 'preparing it here would be the second read again')

  /*
   * The same value everywhere, not just read once. Auth0 provisioning builds callback URLs from
   * STACK_DOMAIN and is not idempotent; taking it from process.env would let an exported override beat
   * the file, so Auth0 would be configured for one domain and every deploy would use another.
   */
  assert.doesNotMatch(
    source,
    /process\.env\.STACK_DOMAIN/,
    'STACK_DOMAIN must come from the validated snapshot, so Auth0 and the store cannot disagree',
  )
  // Exactly one read in the whole script, and it is the one main() validates.
  assert.equal(
    source.match(/readFileSync\(requireStageEnvFile\(\)/g)?.length,
    1,
    'the stage .env must be read in exactly one place',
  )
})

test('the deploy role cannot rewrite its own permissions', () => {
  /*
   * The role is named boxlite-<stage>-github-deploy, so it matches the `role/boxlite-<stage>-*` resource of
   * its own IAM grants. iam:PutRolePolicy on itself is unbounded privilege escalation — the
   * permissions boundary constrains the roles SST creates, not this role's own inline policy — and
   * iam:PutRolePermissionsBoundary would let it lift that boundary for everything else.
   *
   * Derived from what is granted rather than listing actions here: adding a role-mutating action to
   * the Allows must fail until the Deny covers it too, which a hand-written list would not do.
   */
  const statements = deployRolePolicyStatements()
  const asArray = (value: any) => (Array.isArray(value) ? value : [value])
  const SELF_ARN = 'arn:aws:iam::<ACCOUNT>:role/boxlite-<STAGE>-github-deploy'
  const BOUNDARY_ARN = 'arn:aws:iam::<ACCOUNT>:policy/boxlite-<STAGE>-runtime-boundary'
  const SELF_MUTATING = /^iam:(Attach|Detach|Put|Delete|Update)/i

  const granted = new Set<string>()
  for (const statement of statements) {
    if (statement.Effect !== 'Allow') continue
    /*
     * Glob against this role's own ARN, matching how the Deny side below is checked.
     *
     * This was a literal `role/boxlite-*` search, which stopped matching anything once #1255 scoped
     * the Allows to `role/boxlite-<STAGE>-*` — a pattern that still reaches this role.
     * The premise assertion below catches that and fails, so the risk was never a silent pass; it was
     * a loud failure whose obvious "fix" is to widen the grants back until the string matches again.
     *
     * The glob also credits statements whose resource is `*`, which the literal never did. Those
     * reach this role as surely as a named pattern, so the Denies they require are now demanded too.
     */
    const reachesOwnName = asArray(statement.Resource).some(
      (resource: any) => typeof resource === 'string' && iamGlob(resource).test(SELF_ARN),
    )
    if (!reachesOwnName) continue
    for (const action of asArray(statement.Action)) {
      if (typeof action !== 'string' || !SELF_MUTATING.test(action)) continue
      // An instance-profile action cannot be aimed at a role ARN, so denying it on this role's own
      // ARN would be inert. The deploy role has no instance profile of its own to protect.
      if (action.includes('InstanceProfile')) continue
      granted.add(action)
    }
  }
  assert.ok(
    granted.size > 0,
    'expected the role to hold role-mutating actions reaching its own ARN; the premise here has changed',
  )

  const denied = new Set<string>()
  for (const statement of statements) {
    if (statement.Effect !== 'Deny') continue
    /*
     * A Deny only counts if it actually lands. `…github-deploy-other` ends with a suffix and names a
     * different role, and a Condition can make the Deny apply in cases that never occur — so require
     * the resource to end at the role's own name, and refuse to credit a conditional Deny at all.
     */
    if (statement.Condition !== undefined) continue
    // Glob, not endsWith: the Deny names role/boxlite-*-github-deploy so it protects sibling stages
    // as well, and a literal comparison against this stage's ARN would not see that it covers it.
    const coversSelf = asArray(statement.Resource).some(
      (resource: any) => typeof resource === 'string' && iamGlob(resource).test(SELF_ARN),
    )
    if (!coversSelf) continue
    for (const action of asArray(statement.Action)) denied.add(action)
  }

  const uncovered = [...granted].filter((action) => !denied.has(action))
  assert.deepEqual(
    uncovered,
    [],
    'these actions can be aimed at the deploy role itself with no Deny covering it: ' + uncovered.join(', '),
  )
  assert.ok(denied.has('iam:PutRolePermissionsBoundary'), 'the role must not be able to set its own boundary')
  assert.ok(denied.has('iam:DeleteRolePermissionsBoundary'), 'the role must not be able to drop its own boundary')

  /*
   * The other half, and the one that reads as harmless: the runtime boundary is a managed policy named
   * boxlite-<stage>-runtime-boundary, which matches ManageBoxLitePolicies' policy/boxlite-<stage>-*
   * resource.
   * Widen it, create a bounded role under the widened version, pass it to a service, and the boundary
   * has stopped bounding anything. Denying writes to that one policy is what breaks the chain.
   */
  const deniedOnBoundary = new Set<string>()
  for (const statement of statements) {
    if (statement.Effect !== 'Deny' || statement.Condition !== undefined) continue
    const coversBoundary = asArray(statement.Resource).some(
      (resource: any) => typeof resource === 'string' && iamGlob(resource).test(BOUNDARY_ARN),
    )
    if (!coversBoundary) continue
    for (const action of asArray(statement.Action)) deniedOnBoundary.add(action)
  }
  for (const action of ['iam:CreatePolicyVersion', 'iam:SetDefaultPolicyVersion', 'iam:DeletePolicy']) {
    assert.ok(deniedOnBoundary.has(action), `${action} on the runtime boundary must be denied`)
  }

  /*
   * And a SIBLING stage's deploy role. No Allow reaches one any more — #1255 scoped them all to
   * <STAGE> — so this Deny is a backstop rather than a live patch, and it is asserted
   * for the same reason the policy keeps it cross-stage: the next grant that widens should hit a
   * Deny already in place instead of needing one written under time pressure.
   */
  const siblingArn = SELF_ARN.replace('<STAGE>', 'some-other-stage')
  const deniedOnSibling = statements
    .filter((statement: any) => statement.Effect === 'Deny' && statement.Condition === undefined)
    .filter((statement: any) =>
      asArray(statement.Resource).some(
        (resource: any) => typeof resource === 'string' && iamGlob(resource).test(siblingArn),
      ),
    )
    .flatMap((statement: any) => asArray(statement.Action))
  for (const action of ['iam:PutRolePolicy', 'iam:UpdateAssumeRolePolicy']) {
    assert.ok(deniedOnSibling.includes(action), `${action} on another stage's deploy role must be denied`)
  }

  // The boundary gets the same backstop for the same reason, one level down: nothing grants a dev job
  // reach over prod's boundary today, and this is what keeps that true if something regains it.
  const siblingBoundaryArn = BOUNDARY_ARN.replace('<STAGE>', 'some-other-stage')
  const deniedOnSiblingBoundary = statements
    .filter((statement: any) => statement.Effect === 'Deny' && statement.Condition === undefined)
    .filter((statement: any) =>
      asArray(statement.Resource).some(
        (resource: any) => typeof resource === 'string' && iamGlob(resource).test(siblingBoundaryArn),
      ),
    )
    .flatMap((statement: any) => asArray(statement.Action))
  for (const action of ['iam:CreatePolicyVersion', 'iam:SetDefaultPolicyVersion']) {
    assert.ok(
      deniedOnSiblingBoundary.includes(action),
      `${action} on another stage's runtime boundary must be denied`,
    )
  }
})

test('the grants that are NOT stage-scoped are the documented ones, and only those', () => {
  /*
   * The isolation test above says what the deploy role cannot reach. This says what it still can, so
   * the two together are the whole picture: a grant that is account-wide because its resource has no
   * stage to scope to is a documented limit, and a new one appearing silently is not.
   *
   * Pinned against docs/security.md, because a limit nobody wrote down is indistinguishable from one
   * nobody noticed.
   */
  // The same statements the isolation test above reads.
  const statements = deployRolePolicyStatements()
  const asArray = (value: any) => (Array.isArray(value) ? value : [value])
  const stageScoped = (resource: any) => String(resource).includes('<STAGE>')

  /*
   * Every resource that names no stage, excluding the SST backing store the other test covers.
   *
   * Allow only, because this test is about reach: a Deny naming a stage-less resource does not widen
   * anything, it narrows. DenySelfPrivilegeEscalation deliberately spans every stage — that is the
   * whole point of it — and counting it here would demand an excuse for the statement whose job is
   * protecting sibling stages. Before #1255 that excuse existed by accident, because the entries
   * covering the account-wide Allows matched the Deny's resources as substrings too.
   */
  const unscoped = statements
    .filter((statement: any) => statement.Effect === 'Allow')
    .flatMap((statement: any) =>
    asArray(statement.Resource)
      .filter((resource: any) => typeof resource === 'string' && !stageScoped(resource))
      .filter((resource: string) => !resource.includes('sst-state-') && !resource.includes('/sst/'))
      .map((resource: string) => ({ sid: statement.Sid, resource })),
  )

  /*
   * Every resource this role can reach across stages, and why each one cannot be scoped. The list is
   * the point: adding a grant whose resource carries no stage should require adding a line here and
   * saying why, rather than passing unnoticed.
   */
  // The three whose reach across stages is a property of this change's story, so security.md has to
  // keep saying so — a limit recorded only in a test is a limit the next reader will not find.
  const DOCUMENTED_IN_SECURITY_MD: Array<[string, string]> = [
    ['sst-asset-', 'deployment assets are content-addressed; the key is the content hash'],
    ['boxlite-volume-', 'the bucket name carries no stage — scoping it is a rename'],
    [':instance/*', 'an EC2 instance ARN carries no stage; narrowing needs a tag Condition'],
    ['identity/*', 'an SES identity ARN carries the sender domain, not the stage'],
  ]
  /*
   * The rest predate this change and are accounted for here rather than in the prose.
   *
   * The three IAM patterns that used to sit here — role/, instance-profile/ and policy/ on
   * `boxlite-*` — are deliberately gone (#1255). Their excuse was "SST names the roles it creates per
   * stack, and the stage is not a prefix", which was the wrong reason to keep an account-wide grant.
   * What SST actually does (.sst/platform/src/components/component.ts): roles are in namingRules
   * (:257) and get `<app>-<stage>-` when created as a component's child, so boxlite-dev-ApiExecutionRole-*
   * is reachable by a stage pattern; a role at stack root autonames instead (`RunnerRole-1115ba6`);
   * and instance profiles and managed policies are on the skip list at :142-143, so SST never
   * prefixes those at all. None of the three was ever matched by `boxlite-*` either, so the broad
   * pattern bought nothing over the narrow one. Leaving these entries would let a revert to the
   * account-wide form pass in silence, which is how they lasted this long.
   */
  const ACCOUNTED_HERE: Array<[string, string]> = [
    ['document/AWS-RunShellScript', 'an AWS-owned SSM document, not a stage resource'],
    ['role/aws-service-role/', 'service-linked roles are account-global by AWS design'],
  ]
  /*
   * `*` carries no resource name to match on, so it is allowed per statement rather than per pattern.
   * Excluding it wholesale — which this did at first — would have let the single widest grant there is
   * appear without failing anything.
   */
  const WILDCARD_RESOURCE_STATEMENTS: Array<[string, string]> = [
    ['BoxLiteAwsControlPlane', 'the control-plane calls SST makes have no resource-level ARNs'],
    ['RunnerCommandStatus', 'SSM command-history APIs are list-shaped and take no resource'],
    ['ReadIamAndAccountMetadata', 'account and IAM reads SST performs before it knows any resource'],
    ['ReadMailIdentityVerification', 'the classic SES verification read takes no resource ARN'],
  ]

  /*
   * A Sid is a label, so allowing one by name is not enough on its own: `ReadIamAndAccountMetadata`
   * could keep its name and gain `iam:PutRolePolicy` on `*`, which no other check here would see.
   * Nothing granted account-wide may write IAM or grant everything.
   */
  /*
   * Secrets Manager belongs on this list for the same reason IAM does, and its absence is how
   * `secretsmanager:*` on `*` sat here unremarked until #1255: every secret this stack creates carries
   * the stage in its name, so anything beyond the calls that genuinely take no resource is reach into
   * another stage. Named rather than pattern-matched, because "takes no resource" is a property of the
   * specific API, not of its spelling — a new one has to be added here deliberately.
   */
  const RESOURCELESS_SECRETSMANAGER = ['secretsmanager:ListSecrets', 'secretsmanager:GetRandomPassword']

  for (const [sid] of WILDCARD_RESOURCE_STATEMENTS) {
    const statement = statements.find((candidate: any) => candidate.Sid === sid)
    assert.ok(statement, `${sid} is allowed a wildcard resource but no longer exists`)
    const dangerous = asArray(statement.Action).filter(
      (action: any) =>
        typeof action === 'string' &&
        (action === '*' ||
          /^iam:(?!Get|List|Simulate)/i.test(action) ||
          /^sts:AssumeRole/i.test(action) ||
          (/^secretsmanager:/i.test(action) && !RESOURCELESS_SECRETSMANAGER.includes(action))),
    )
    assert.deepEqual(dangerous, [], `${sid} grants ${dangerous.join(', ')} on every resource`)
  }

  const known = [...DOCUMENTED_IN_SECURITY_MD, ...ACCOUNTED_HERE]
  const undocumented = unscoped.filter(({ sid, resource }: any) =>
    resource === '*'
      ? !WILDCARD_RESOURCE_STATEMENTS.some(([allowedSid]) => allowedSid === sid)
      : !known.some(([pattern]) => resource.includes(pattern)),
  )
  assert.deepEqual(undocumented, [], 'an account-wide resource grant appeared that nothing accounts for')

  /*
   * And the statement the account-wide secretsmanager grant was replaced by, pinned the way the
   * boundary's four statements are. The check above catches re-widening — a `*` resource here would
   * surface as undocumented — but not deletion, which would leave the deploy unable to read its own
   * secrets and nothing failing until a deploy actually ran.
   */
  assert.deepEqual(findStatement(statements, 'SecretsForThisStage'), {
    Sid: 'SecretsForThisStage',
    Effect: 'Allow',
    Action: 'secretsmanager:*',
    Resource: ['arn:aws:secretsmanager:<REGION>:<ACCOUNT>:secret:boxlite-<STAGE>-*'],
  })

  /*
   * And the secret the stack actually creates has to land inside that pattern, which is a contract
   * between two files nothing else checks. It is not automatic: SST's `<app>-<stage>-` prefix comes
   * from a transformation registered in its Component constructor, so a resource declared at stack
   * root autonames instead — `RunnerRole-1115ba6` and `RunnerProfile-434704b` are live proof. An
   * autonamed secret sits outside a stage-scoped grant, and the deploy fails with AccessDenied the
   * first time GHCR_TOKEN is set. That is a runtime failure no unit test would otherwise reach.
   */
  const runnersSource = liveText('script', readFileSync(join(REPO_ROOT, 'apps/infra/stack/runners.ts'), 'utf8'))
  const secretName = runnersSource.match(/new aws\.secretsmanager\.Secret\([^)]*?name: `([^`]+)`/s)?.[1]
  assert.ok(secretName, 'the stack must name its Secrets Manager secret explicitly, not let it autoname')
  const composedSecretName = secretName.replace('${$app.name}', 'boxlite').replace('${$app.stage}', 'dev')
  assert.ok(
    iamGlob(
      'arn:aws:secretsmanager:ap-southeast-1:123456789012:secret:boxlite-dev-*',
    ).test(`arn:aws:secretsmanager:ap-southeast-1:123456789012:secret:${composedSecretName}`),
    `the stack's secret '${composedSecretName}' falls outside the deploy role's secret:boxlite-<stage>-* grant`,
  )

  /*
   * Both directions. "Every unscoped grant is documented" alone lets the documentation rot the other
   * way: narrow or delete boxlite-volume-* later and security.md would still describe it as shared,
   * with nothing failing. So each documented pattern must also still match a real unscoped resource.
   */
  const security = readFileSync(join(REPO_ROOT, 'apps/infra/docs/security.md'), 'utf8')
  for (const [pattern] of DOCUMENTED_IN_SECURITY_MD) {
    const claim = pattern.replace(/^:/, '').replace(/-$/, '-*')
    assert.ok(security.includes(claim), `docs/security.md must still name ${claim} as a shared grant`)
    assert.ok(
      unscoped.some(({ resource }: any) => resource.includes(pattern)),
      `docs/security.md still describes ${claim} as reaching every stage, but no grant matches it — ` +
        'either the policy was narrowed and the note is now wrong, or the pattern here is stale',
    )
  }
})

test('every step that runs the sst wrapper is given the Cloudflare credentials', () => {
  /*
   * The wrapper falls back to an SSM copy when the variables are unset, and whether this role can
   * decrypt that parameter has never been verified — so a step without them either works by a path
   * nobody has confirmed, or fails at provider initialization. Both are avoidable by passing the
   * Environment secrets the stage already holds.
   *
   * `install` counts: it evaluates sst.config.ts, which initializes every provider declared there.
   */
  const workflow: any = load(readFileSync(DEPLOY_WORKFLOW, 'utf8'))
  const steps = values(workflow.jobs).flatMap((job: any) => job.steps ?? [])
  const wrapperSteps = steps.filter((step: any) => typeof step.run === 'string' && /npm run .*\bsst\b/.test(step.run))
  assert.ok(wrapperSteps.length > 0, 'no step runs the sst wrapper; this test is looking at the wrong thing')

  for (const step of wrapperSteps) {
    for (const secret of ['CLOUDFLARE_API_TOKEN', 'CLOUDFLARE_DEFAULT_ACCOUNT_ID']) {
      assert.ok(step.env?.[secret], `step "${step.name}" runs sst without ${secret}`)
    }
  }
})

test('every unusable stage configuration store stops the deploy rather than warning', () => {
  // Over live source, for the reason the API-image test gives: nothing executes this path in a test,
  // so a branch downgraded to a warning would leave every unit test passing. The store is now the only
  // source of a stage's configuration, so "read it, and continue if that failed" would deploy against
  // defaults — a stage silently reconciled to something nobody wrote.
  const source = liveText('script', readFileSync(SST_WRAPPER, 'utf8'))
  const start = source.indexOf('function loadStageConfig(')
  assert.notEqual(start, -1, 'loadStageConfig is missing from the wrapper')
  // To the function's own closing brace at column 0 — every declaration here is top-level, and a
  // boundary guessed at the next function would silently count another one's branches.
  const body = source.slice(start, source.indexOf('\n}\n', start))

  // Unreadable, unbootstrapped, torn, half-written, applying-nothing, and removed configuration.
  // Six refusals, each an error
  // paired with an exit: downgrading any one to a warning drops both counts and fails here.
  assert.equal(body.match(/process\.exit\(1\)/g)?.length, 6, 'a store the wrapper cannot use must exit')
  assert.equal(body.match(/console\.error\(/g)?.length, 6, 'every refusal must say which one it is')
  // Exactly one, and it is not a refusal: an unlisted key is a stale or hand-written entry that will
  // never take effect, worth surfacing but not worth failing a deploy over.
  assert.equal(body.match(/console\.warn\(/g)?.length, 1, 'only the unlisted-keys notice may warn')

  // `diff` reads only, but a preview built from defaults is not a preview of the apply that follows —
  // the operator approves that plan and the apply then reads the store successfully.
  assertLiveLine(source, /STAGE_CONFIG_SUBCOMMANDS = new Set\(\[[^\]]*'diff'/)

  /*
   * No refusal may echo a value. Every one of them names keys — missing.join, manifestNames(stored) —
   * and a future edit adding "…but the store holds <value>" would put a live credential in CI logs,
   * which is exactly the leak the DEPLOY_ENV design was criticized for. `stored[` and `apply[` are the
   * two ways a value gets into that string, so neither may appear in the messages.
   */
  for (const message of body.match(/console\.error\([\s\S]*?\)\n/g) ?? []) {
    assert.doesNotMatch(message, /stored\[|apply\[/, 'a refusal must report key names, never a value')
  }
})

test('SST deploy does not depend on a laptop-managed remote builder', () => {
  const source = readFileSync(SST_WRAPPER, 'utf8')
  const packageSource = readFileSync(join(REPO_ROOT, 'apps/infra/package.json'), 'utf8')

  assert.doesNotMatch(source, /RemoteAmd64Builder/)
  assert.doesNotMatch(source, /BUILDX_BUILDER/)
  assert.doesNotMatch(packageSource, /builder:(?:provision|start|status|stop)/)
  for (const legacyPath of [
    'apps/infra/scripts/buildx-builder.mjs',
    'apps/infra/scripts/buildx-builder-cli.mjs',
    'apps/infra/buildkit/amd64-builder.yaml',
    'apps/infra/buildkit/buildkitd.toml',
  ]) {
    assert.equal(existsSync(join(REPO_ROOT, legacyPath)), false, `${legacyPath} must be removed`)
  }
})

test('the capability gate answers every selected-commit shape', () => {
  /*
   * The gate is shell inside YAML, so a contract assertion could only prove it is present. This runs
   * the step's own script against the trees a dispatch can select: one declaring the capabilities,
   * one from before each existed, one whose file is corrupt, and one with no file at all. Every case
   * but the first must refuse — a parse failure or a missing file falling through to the happy path
   * would deploy exactly the commit the gate exists to catch.
   *
   * One version set covers both capabilities on purpose. capabilities.json is one document with one
   * version, so a bump made for an unrelated capability must not read as "this commit cannot use the
   * store" (#1253).
   */
  const workflow = load(readFileSync(DEPLOY_WORKFLOW, 'utf8'))
  const gate = workflow.jobs.deploy.steps.find(
    (step: any) => step.name === 'Require deployment capabilities in the selected commit',
  )
  assert.ok(gate, 'the deployment capability gate is missing')

  const checkout = mkdtempSync(join(tmpdir(), 'boxlite-capability-gate-'))
  const capability = join(checkout, 'apps/infra/deployment/capabilities.json')
  mkdirSync(dirname(capability), { recursive: true })

  const runGate = (contents: string | null, exclude: string) => {
    if (contents === null) rmSync(capability, { force: true })
    else writeFileSync(capability, contents)
    return spawnSync('/usr/bin/env', ['bash', '-c', gate.run], {
      cwd: checkout,
      encoding: 'utf8',
      env: { ...process.env, BOXLITE_ARTIFACT_REF: 'fixture-commit', DEPLOY_EXCLUDE: exclude },
    })
  }
  const declared = (extra: object) => JSON.stringify({ stageConfigStore: true, componentSelection: true, ...extra })

  try {
    for (const version of [1, 2]) {
      for (const exclude of ['', 'Api', 'Runner']) {
        const accepted = runGate(declared({ version }), exclude)
        assert.equal(accepted.status, 0, `capability v${version} must deploy with exclude='${exclude}': ${accepted.stderr}`)
      }
    }

    /*
     * Each refusal is checked by the cause it reports, not only by its exit status. The three
     * causes are answered by different arms of the gate and differ solely in what they tell the
     * operator, so asserting the status alone would let the arms swap messages unnoticed — and
     * "this commit is too old" sends someone to redispatch a newer ref when the real problem is a
     * capability file they need to fix.
     */
    const refused = (contents: string | null, exclude: string) => {
      const result = runGate(contents, exclude)
      assert.notEqual(result.status, 0, `expected a refusal for ${contents}`)
      return result.stderr
    }

    // A commit that cannot read the store would deploy against whatever defaults survived.
    assert.match(refused(JSON.stringify({ version: 1, componentSelection: true }), ''), /does not declare the capabilities/)
    // Component selection is only required when the dispatch actually narrows the scope.
    assert.equal(runGate(JSON.stringify({ version: 1, stageConfigStore: true }), '').status, 0)
    assert.match(refused(JSON.stringify({ version: 1, stageConfigStore: true }), 'Runner'), /--exclude Runner/)
    // A version this workflow has never been taught to read.
    assert.match(refused(declared({ version: 3 }), ''), /does not declare the capabilities/)
    // Corrupt, then absent. Each has its own cause, and neither may borrow the other's: a parse
    // failure is not an age problem, and a missing file is not a corrupt one.
    const corrupt = refused('{ not json', '')
    assert.match(corrupt, /failed to load/)
    assert.doesNotMatch(corrupt, /does not declare the capabilities/)
    const absent = refused(null, '')
    assert.match(absent, /declares no/)
    assert.doesNotMatch(absent, /failed to load/)
  } finally {
    rmSync(checkout, { recursive: true, force: true })
  }
})

test('deployment previews and reconciles the full stack in guarded GitHub CI', () => {
  assert.ok(existsSync(DEPLOY_WORKFLOW), 'the stack deployment workflow is missing')
  const source = readFileSync(DEPLOY_WORKFLOW, 'utf8')
  const workflow = load(source)
  const safetyTestStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Run deployment safety tests')
  const boundaryCheckStep = workflow.jobs.deploy.steps.find(
    (step: any) => step.name === 'Verify deploy role IAM boundary permissions',
  )
  const installStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Install SST providers')
  const previewStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Preview the selected components')
  const deployStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Deploy the selected components')

  assert.match(source, /workflow_dispatch:/)
  assert.equal(workflow.on.workflow_dispatch.inputs.stage.type, 'choice')
  assert.ok(workflow.on.workflow_dispatch.inputs.stage.options.includes('dev'))
  assert.deepEqual(workflow.on.workflow_dispatch.inputs.apply, {
    description: 'Preview again, then deploy the selected components',
    required: true,
    default: false,
    type: 'boolean',
  })
  assert.match(source, /environment: \$\{\{ inputs\.stage \}\}/)
  assert.equal(workflow.permissions['id-token'], 'write')
  assert.equal(workflow.jobs.deploy['runs-on'], 'ubuntu-24.04')

  // `if:` restricts the workflow ref, not the dispatched `ref`/`pr` inputs — this shell guard is
  // the only thing binding the built commit to main or to a pull request someone is proposing.
  // Read the step it lives in, since demoting a line to a comment leaves it greppable while the
  // guard is gone.
  const refGuardStep = workflow.jobs['resolve-ref'].steps.find(
    (step: any) => step.name === 'Require a commit on main or an open pull request',
  )
  assert.ok(refGuardStep, 'the deployable-commit guard step is missing')
  // Anchored per line with no leading `#`: a parsed read still hands back the whole shell body,
  // so commenting a check out leaves it matchable while it no longer runs.
  assert.match(refGuardStep.run, /^\s*set -euo pipefail/m)
  assert.match(refGuardStep.run, /^\s*\[ -z "\$INPUT_REF" \] \|\| \[ -z "\$INPUT_PR" \] \|\| \{/m)
  assert.match(refGuardStep.run, /ref and pr are mutually exclusive/)
  // The PR path resolves by NUMBER, not by asking the API which PR (if any) owns a given SHA —
  // that lookup (/commits/{sha}/pulls) returns an empty array for a fork PR's head, so no fix to
  // it could ever accept a fork. gh pr view has no such gap: it works identically for a same-repo
  // or a fork PR, which is the whole point of resolving this direction instead of the other.
  assert.match(refGuardStep.run, /^\s*\[\[ "\$INPUT_PR" =~ \^\[0-9\]\+\$ \]\]/m)
  assert.match(
    refGuardStep.run,
    /^\s*pr_json="\$\(gh pr view "\$INPUT_PR" --json state,headRefOid,isCrossRepository,mergeable,potentialMergeCommit\)"/m,
  )
  assert.match(refGuardStep.run, /^\s*\[ "\$state" = "OPEN" \] \|\| \{/m)
  // The MERGE commit, not the head. The workflow definition comes from main while this job picks
  // what is checked out, so deploying a head pairs main's YAML with whatever tooling that branch
  // carries — the mismatch that sent a components=api dispatch into an apps/infra with no scope
  // support at all. refs/pull/N/merge is main+PR by construction, so it cannot be behind main.
  assertShellLine(refGuardStep.run, /sha="\$\(jq -r '\.potentialMergeCommit\.oid \/\/ empty' <<<"\$pr_json"\)"/)
  assertShellLine(refGuardStep.run, /head_sha="\$\(jq -r '\.headRefOid' <<<"\$pr_json"\)"/)
  // A conflicting PR has no merge commit, and an uncomputed one is a "not yet" rather than a
  // verdict — distinct causes, so distinct refusals. Emitting the head as a fallback would
  // silently reintroduce exactly the behaviour this replaces.
  assertShellLine(refGuardStep.run, /\[ "\$mergeable" != "CONFLICTING" \] \|\| \{/)
  assertShellLine(refGuardStep.run, /\[ -n "\$sha" \] \|\| \{/)
  // Mergeability is computed lazily, so a cold cache answers UNKNOWN and the merge SHA is empty.
  // Both the loop AND its re-query are pinned: without the re-query the loop spins over the same
  // stale JSON, which fails a dispatch that one refresh would have resolved and makes the "after
  // 5 attempts" message untrue.
  assertShellLine(refGuardStep.run, /for attempt in 1 2 3 4 5; do/)
  assertShellLine(refGuardStep.run, /\[ "\$mergeable" = "UNKNOWN" \] \|\| \[ -z "\$sha" \] \|\| break/)
  const retryBody = liveShell(refGuardStep.run)
  const loopStart = retryBody.indexOf('for attempt in 1 2 3 4 5; do')
  assert.match(
    retryBody.slice(loopStart, retryBody.indexOf('done', loopStart)),
    /pr_json="\$\(gh pr view/,
    'the retry loop must re-query, or it polls its own stale answer',
  )
  // Raw, not live: the message embeds `PR #$INPUT_PR`, and liveShell's stripper reads a `#`
  // preceded by whitespace as a comment and deletes the rest of the line. The guards above are
  // pinned live — they carry the behaviour; these two only pin that each cause says its own name.
  assert.match(refGuardStep.run, /conflicts with main, so it has no merge commit to deploy/)
  assert.match(refGuardStep.run, /has no merge commit yet \(mergeable=\$mergeable\)/)
  assert.doesNotMatch(
    liveShell(refGuardStep.run),
    /sha="\$head_sha"/,
    'the head must never stand in for a missing merge commit',
  )
  // PR #1148 refused a fork head deliberately (`.head.repo.full_name == GITHUB_REPOSITORY`), a
  // named security boundary — not a side effect of the SHA-reverse-lookup bug this guard fixes.
  // This guard drops that boundary on purpose: isCrossRepository is fetched and logged for
  // whoever reviews the run, but nothing here may branch on it. Pin the shape (fetched, echoed)
  // AND the absence (no `exit 1` between computing it and the block's `exit 0`) so a fork
  // exclusion added back later doesn't silently pass this test by accident.
  assert.match(refGuardStep.run, /^\s*fork="\$\(jq -r '\.isCrossRepository' <<<"\$pr_json"\)"/m)
  assert.match(
    refGuardStep.run,
    /^\s*echo "PR #\$INPUT_PR \(\$\(\[ "\$fork" = "true" \] && echo fork \|\| echo same-repo\)\) head is \$head_sha; deploying merge \$sha"/m,
  )
  const forkOnwards = refGuardStep.run.slice(refGuardStep.run.indexOf('fork="$(jq'))
  const prBlockTail = forkOnwards.slice(0, forkOnwards.indexOf('exit 0') + 'exit 0'.length)
  assert.doesNotMatch(
    prBlockTail,
    /exit 1/,
    'a fork PR head must not be rejected between resolving it and exit 0 — same-repo and fork PRs are accepted identically',
  )
  assert.match(refGuardStep.run, /^\s*echo "sha=\$sha" >> "\$GITHUB_OUTPUT"/m)
  // The main-commit path is untouched by the pr path above it.
  assert.match(refGuardStep.run, /^\s*\[\[ "\$candidate" =~ \^\[0-9a-f\]\{40\}\$ \]\]/m)
  assert.match(refGuardStep.run, /^\s*\|\| ! git merge-base --is-ancestor "\$candidate" origin\/main/m)
  // The API the pr path depends on; without it every `pr` input 404s, which fails closed (the
  // guard rejects), not open — but it's still the reason this permission is here.
  assert.equal(workflow.jobs['resolve-ref'].permissions['pull-requests'], 'read')
  assert.equal(workflow.jobs['resolve-ref'].permissions.contents, 'read')
  assert.deepEqual(workflow.jobs['resolve-ref'].outputs, { sha: '${{ steps.ref.outputs.sha }}' })

  // The reusable builds and what they are told to build: `with:` values decide which commit and
  // which C SDK the Runner links, and build-runner-binary.yml defaults libboxlite_source to the
  // published release, so an absent input silently links the wrong artifact.
  assert.equal(workflow.jobs['build-c'].uses, './.github/workflows/build-c.yml')
  assert.equal(workflow.jobs['build-c'].with.linux_x64_only, true)
  assert.equal(workflow.jobs['build-c'].with.ref, '${{ needs.resolve-ref.outputs.sha }}')
  assert.equal(workflow.jobs['build-runner'].uses, './.github/workflows/build-runner-binary.yml')
  assert.equal(workflow.jobs['build-runner'].with.libboxlite_source, 'artifact')
  assert.equal(workflow.jobs['build-runner'].with.ref, '${{ needs.resolve-ref.outputs.sha }}')
  assert.equal(workflow.jobs['build-api'].uses, './.github/workflows/build-apps-api-image.yml')
  assert.equal(workflow.jobs['build-api'].with.ref, '${{ needs.resolve-ref.outputs.sha }}')
  assert.equal(workflow.jobs['build-api'].with.stage, '${{ inputs.stage }}')

  // The Api leg links against nothing the C SDK produces. Adding build-c to its `needs` would
  // still deploy the right bytes, just serialized behind a Rust compile it never reads.
  assert.deepEqual(workflow.jobs['build-api'].needs, 'resolve-ref')

  // The suite that proves the deploy is the third call of that shape, and the two values that
  // make it worth running are just as much contract. Drop `with.ref` and it builds its SDKs from
  // tip-of-main against whatever commit was deployed; drop the apply gate and it spends dev-stack
  // capacity re-testing a stack the run only previewed.
  assert.equal(workflow.jobs.e2e.uses, './.github/workflows/e2e-cloud.yml')
  assert.equal(workflow.jobs.e2e.with.ref, '${{ needs.resolve-ref.outputs.sha }}')
  assert.equal(workflow.jobs.e2e.if, '${{ inputs.apply }}')
  // Named, not `inherit` — which would hand the suite every secret this job can reach.
  assert.equal(workflow.jobs.e2e.secrets.BOXLITE_DEV_API_KEY, '${{ secrets.BOXLITE_DEV_API_KEY }}')
  // `needs` carries the ordering the `if` relies on: no status-check function appears in that
  // expression, so the default success() is what stops the suite running behind a failed deploy.
  assert.deepEqual(workflow.jobs.e2e.needs, ['resolve-ref', 'deploy'])

  // A narrowed scope must drop the build AND the reconcile, or it is not a scope. Each half is
  // pinned separately because either alone is a distinct bug: the builds without the exclusion
  // deploys a component from a commit it was never built for, and the exclusion without the
  // build gating just burns the CI time the input exists to save.
  const components = workflow.on.workflow_dispatch.inputs.components
  assert.equal(components.type, 'choice', 'components must be an allowlist, not free text')
  assert.deepEqual(components.options, ['api+runner', 'api', 'runner'])
  assert.equal(components.default, 'api+runner', 'an unqualified dispatch must still deploy everything')
  // `contains` reads as membership only while no single-leg option contains the other leg's name.
  // The combined option contains both by design; a leg that contained the other would make its
  // gate fire for a scope that excludes it — an `api`-only dispatch building the Runner anyway.
  const legs = components.options.filter((option: any) => option !== 'api+runner')
  for (const leg of legs) {
    for (const other of legs.filter((candidate: any) => candidate !== leg)) {
      assert.ok(!leg.includes(other), `option '${leg}' contains '${other}', which breaks the contains() gates`)
    }
    assert.ok(components.default.includes(leg), `the default must select '${leg}'`)
  }
  assert.equal(workflow.jobs['build-api'].if, "${{ contains(inputs.components, 'api') }}")
  assert.equal(workflow.jobs['build-c'].if, "${{ contains(inputs.components, 'runner') }}")
  assert.equal(workflow.jobs['build-runner'].if, "${{ contains(inputs.components, 'runner') }}")
  for (const stepName of ['Download commit Runner artifact', 'Stage commit Runner artifact']) {
    const step = workflow.jobs.deploy.steps.find((candidate: any) => candidate.name === stepName)
    assert.ok(step, `${stepName} is missing`)
    assert.equal(step.if, "${{ contains(inputs.components, 'runner') }}", `${stepName} must be scope-gated`)
  }
  // The SST component each scope excludes. `--target` must never appear: it omits the shared and
  // provider resources a partial update still depends on, which is how PR #1095 stalled the stack
  // mid-provider-migration. deployment/scope.ts rejects it, and this keeps the workflow honest
  // before it ever gets there.
  assert.equal(
    workflow.jobs.deploy.env.DEPLOY_EXCLUDE,
    "${{ inputs.components == 'api' && 'Runner' || inputs.components == 'runner' && 'Api' || '' }}",
  )
  assert.doesNotMatch(liveShell(source), /--target/)
  // The workflow definition comes from the dispatch ref while this job checks out the SELECTED
  // commit, so the two are versioned independently and `--exclude` is the first thing that
  // couples them. Observed: run 31229121181 dispatched `components=api` at a PR head predating
  // component selection, and the old wrapper answered `partial SST deploys are disabled` — true
  // of that commit, but it reads as a statement about this workflow.
  // What the gate decides, and which cause it reports for each refusal, is exercised for real by
  // 'the capability gate answers every selected-commit shape', which runs this step's own script
  // against each tree a dispatch can select and reads its stderr. Left here: that the step exists,
  // that it reads the capability file rather than this workflow's assumptions, and where it sits.
  const capabilityGate = workflow.jobs.deploy.steps.find(
    (step: any) => step.name === 'Require deployment capabilities in the selected commit',
  )
  assert.ok(capabilityGate, 'the deployment capability gate is missing')
  assertShellLine(capabilityGate.run, /capability=apps\/infra\/deployment\/capabilities\.json/)
  // Before the deploy role is assumed. An unsupported commit is knowable from the checkout alone,
  // so it must never reach AWS credentials.
  const deployStepNames = workflow.jobs.deploy.steps.map((step: any) => step.name)
  assert.ok(
    deployStepNames.indexOf('Require deployment capabilities in the selected commit') <
      deployStepNames.indexOf('Configure AWS credentials through OIDC'),
    'the capability gate must run before AWS credentials are configured',
  )
  // A skipped build job would cascade a skip to the deploy under the implicit success(). Naming a
  // status-check function turns that off — without one, every narrowed dispatch silently deploys
  // nothing while reporting green.
  assert.match(workflow.jobs.deploy.if, /!cancelled\(\)/)
  assert.match(workflow.jobs.deploy.if, /!contains\(needs\.\*\.result, 'failure'\)/)
  assert.match(workflow.jobs.deploy.if, /!contains\(needs\.\*\.result, 'cancelled'\)/)
  assert.match(workflow.jobs.deploy.if, /github\.ref == 'refs\/heads\/main'/)

  // Both components resolve to the one commit the build jobs actually produced. The stage's store
  // cannot redirect that, at either boundary: bootstrap refuses to store the selector keys
  // (deployableStageConfig), and hydration refuses them even if one is written by hand and named by
  // the manifest — by the allowlist and the local-only denylist alike, so relaxing either one on its
  // own does not open the door. The workflow's own env: block is a real environment variable too,
  // which always beats a stored value.
  for (const selector of ['BOXLITE_ARTIFACT_SOURCE', 'API_ARTIFACT_SOURCE', 'RUNNER_ARTIFACT_SOURCE']) {
    assert.equal(workflow.jobs.deploy.env[selector], 'build', `${selector} must be set on the deploy job`)
  }
  assert.equal(workflow.jobs.deploy.env.BOXLITE_ARTIFACT_REF, '${{ needs.resolve-ref.outputs.sha }}')
  const deployCheckoutStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Checkout selected commit')
  assert.ok(deployCheckoutStep, 'the deploy job never checks out the resolved commit')
  assert.equal(deployCheckoutStep.with.ref, '${{ needs.resolve-ref.outputs.sha }}')
  // Both build legs, not just the Runner's. Dropping build-api would let the deploy resolve a
  // commit image tag whose build had not finished — or never ran — and fail on the pull.
  assert.deepEqual(workflow.jobs.deploy.needs, ['resolve-ref', 'build-api', 'build-runner'])
  const versionStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Resolve commit version')
  assert.ok(versionStep, 'the commit-version step is missing')
  assertShellLine(versionStep.run, /echo "VERSION=\$version" >> "\$GITHUB_ENV"/)

  // Staging decides over the ref, not per key: completing a half-published ref would pair a
  // freshly built (non-byte-identical) manifest with the already-stored tarball, and write-once
  // makes that unrepairable. artifacts/runner-build.ts refuses the same case locally.
  const stageStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Stage commit Runner artifact')
  assert.ok(stageStep, 'the artifact staging step is missing')
  assertShellLine(stageStep.run, /if \[ "\$present" -eq 2 \]; then/)
  assertShellLine(stageStep.run, /elif \[ "\$present" -eq 1 \]; then/)
  assertShellLine(stageStep.run, /is partially published; delete the objects under it and rerun/)
  assertShellLine(stageStep.run, /--if-none-match '\*'/)
  const archStep = workflow.jobs.deploy.steps.find((step: any) => step.name === 'Verify native AMD64 Docker')
  assert.ok(archStep, 'the native-arch guard step is missing')
  assertShellLine(archStep.run, /test "\$\(uname -m\)" = "x86_64"/)
  assertShellLine(archStep.run, /test "\$\(docker info --format '\{\{\.Architecture\}\}'\)" = "x86_64"/)
  assert.match(source, /aws-actions\/configure-aws-credentials@/)
  assertComposedDeployRoleArn(source, '${{ inputs.stage }}')
  // Every sst step passes --stage "$STAGE"; without this job env they would all
  // run with an empty stage.
  assert.equal(workflow.jobs.deploy.env.STAGE, '${{ inputs.stage }}')
  assert.equal(workflow.jobs.deploy.env.IAM_PERMISSIONS_BOUNDARY_STAGE, '${{ inputs.stage }}')
  assertNoMaterializedStageConfig(workflow, source)
  // Any commit on main is deployable, and this job checks out THAT commit's apps/infra. One predating
  // the store still expects apps/infra/.env, so the gate must apply to every deploy rather than only
  // to a narrowed scope.
  assert.equal(capabilityGate.if, undefined, 'the gate must apply to every deploy, not just a narrowed scope')
  assertShellLine(capabilityGate.run, /c\.stageConfigStore === true/)
  // The capabilities this repo declares, so the gate cannot pass against a tree that lacks them.
  const capabilities = JSON.parse(readFileSync(join(REPO_ROOT, 'apps/infra/deployment/capabilities.json'), 'utf8'))
  assert.equal(capabilities.stageConfigStore, true)
  assert.equal(capabilities.componentSelection, true)
  assert.ok(safetyTestStep, 'the deployment safety test step is missing')
  assert.equal(safetyTestStep.run, 'npm test')
  assert.ok(installStep, 'the SST provider installation step is missing')
  assert.equal(installStep.run, 'npm run --silent sst -- install --stage "$STAGE"')
  assert.ok(previewStep, 'the full-stack preview step is missing')
  assert.equal(previewStep.if, undefined, 'Preview validation must not be conditional')
  assert.equal(previewStep['continue-on-error'], undefined, 'Preview failures must stop deployment')
  assert.equal(previewStep.shell, 'bash')
  assert.ok(deployStep, 'the deployment step is missing')
  assert.equal(deployStep.if, '${{ inputs.apply }}')
  /*
   * Five properties per invocation, rather than the script line for line. Each is a way a deploy
   * has actually gone wrong, and none of them cares how the surrounding bash is worded:
   *
   *   - the mandatory Runner policy is passed, so a plan that would replace a Runner is refused;
   *   - the dispatched stage is passed, in the seed. The two legs fail differently without it and
   *     the preview's way is the dangerous one: resolveSstStage returns 'dev' for a non-mutating
   *     verb, so the diff previews a stage the apply never reconciles, while `deploy` refuses
   *     outright (deployment/environment.ts). Pinning the seed keeps both honest;
   *   - the scope is appended conditionally, never inline — `--exclude "$DEPLOY_EXCLUDE"` inline
   *     hands SST an empty component name on every full-scope deploy;
   *   - it is expanded as "${args[@]}", so a scope with a space cannot word-split;
   *   - the array is seeded with the fixed arguments, never `args=()` — expanding an EMPTY array
   *     under `set -u` is an unbound-variable error before bash 4.4, which would kill a full-scope
   *     deploy on the runner's own bash. Verified against bash 3.2.
   */
  for (const [label, step, invocation] of [
    ['preview', previewStep, /npm run --silent sst -- "\$\{args\[@\]\}"/],
    ['deploy', deployStep, /npm run deploy -- "\$\{args\[@\]\}"/],
  ] as const) {
    const shell = liveShell(step.run)
    assert.match(shell, /--policy policies\/runner/, `the ${label} must pass the mandatory Runner policy`)
    const seededArgs = shell.match(/^args=\((.+)\)$/m)
    assert.ok(seededArgs, `the ${label} must seed its argument array`)
    assert.match(seededArgs[1], /--stage "\$STAGE"/, `the ${label} must act on the dispatched stage`)
    assert.match(
      shell,
      /\[ -z "\$DEPLOY_EXCLUDE" \] \|\| args\+=\(--exclude "\$DEPLOY_EXCLUDE"\)/,
      `the ${label} must append the scope only when one was selected`,
    )
    assert.match(shell, invocation, `the ${label} must expand its arguments as a quoted array`)
  }
  // `sst diff` is what makes the preview a preview — the loop above proves it runs under the same
  // policy pack as the apply, so a Runner the plan would replace is refused before approval.
  assert.match(liveShell(previewStep.run), /args=\(diff /)
  assert.ok(
    workflow.jobs.deploy.steps.indexOf(previewStep) < workflow.jobs.deploy.steps.indexOf(deployStep),
    'Preview validation must complete before deployment',
  )
  assert.ok(
    workflow.jobs.deploy.steps.indexOf(safetyTestStep) < workflow.jobs.deploy.steps.indexOf(previewStep),
    'Runner lifecycle contracts must be tested before the deployment preview',
  )
  assert.ok(boundaryCheckStep, 'the IAM boundary preflight step is missing')
  assert.ok(
    workflow.jobs.deploy.steps.indexOf(safetyTestStep) < workflow.jobs.deploy.steps.indexOf(boundaryCheckStep) &&
      workflow.jobs.deploy.steps.indexOf(boundaryCheckStep) < workflow.jobs.deploy.steps.indexOf(previewStep),
    'The IAM boundary preflight must run after safety tests and before the deployment preview',
  )
  assert.ok(
    workflow.jobs.deploy.steps.indexOf(installStep) < workflow.jobs.deploy.steps.indexOf(previewStep),
    'SST providers must be installed before the deployment preview',
  )
  // A selector may name a component only through DEPLOY_EXCLUDE, whose value the `components`
  // choice allowlists. Hardcoding one here would deploy a fixed partial scope on every dispatch,
  // including the default full one — invisible to the input the operator actually set.
  assert.doesNotMatch(
    `${liveShell(previewStep.run)}\n${liveShell(deployStep.run)}`,
    /--(?:target|exclude)[=\s]+(?!"\$DEPLOY_EXCLUDE")[A-Za-z]/,
  )
  assert.doesNotMatch(source, /setup-qemu/)
})

test('the checked-in deploy role satisfies the CI IAM boundary preflight', () => {
  // The preflight gates every deploy, so the document it inspects must actually grant what it
  // looks for. Rendered through aws.ts's own `render` rather than a hand-copied fixture, so a
  // real drift between the two fails here instead of wedging CI on the next run.
  const accountId = '123456789012'
  const stage = 'dev'
  const rendered = render(readFileSync(DEV_DEPLOY_ROLE_POLICY, 'utf8'), { repo: 'boxlite-ai/boxlite', stage, accountId, region: 'ap-southeast-1' })

  const { grants } = verifyDeployRoleGrantsBoundaryPermission({
    callerArn: `arn:aws:sts::${accountId}:assumed-role/boxlite-${stage}-github-deploy/session`,
    accountId,
    stage,
    policyDocuments: [JSON.parse(rendered)],
  })
  assert.equal(grants, true, 'bootstrap/aws/deploy-role-policy.json must grant iam:PutRolePermissionsBoundary for the stage boundary')
})

test('the deploy role grants the CloudFront KeyValueStore prefix Router needs', () => {
  // `cloudfront:*` does not reach `cloudfront-keyvaluestore:*` — an IAM
  // wildcard never crosses the `service:` colon, and these are two service
  // prefixes. sst.aws.Router stores its route table in a KeyValueStore, so
  // without this grant every apply dies on DescribeKeyValueStore while every
  // preview passes, because a preview makes no KV call.
  const actions = readDeployRolePolicy().Statement.flatMap((statement: any) =>
    Array.isArray(statement.Action) ? statement.Action : [statement.Action],
  )
  assert.ok(
    actions.includes('cloudfront-keyvaluestore:*'),
    'bootstrap/aws/deploy-role-policy.json must grant cloudfront-keyvaluestore:*; cloudfront:* does not cover it',
  )
})

test('package scripts disable long-running SST dev for the stateful stack', () => {
  const packageJson = JSON.parse(readFileSync(join(REPO_ROOT, 'apps/infra/package.json'), 'utf8'))

  assert.equal(packageJson.scripts.dev, undefined)
})

test('commit Runner builds consume the C SDK artifact from the same reusable run', () => {
  const cSource = readFileSync(BUILD_C_WORKFLOW, 'utf8')
  const runnerSource = readFileSync(BUILD_RUNNER_WORKFLOW, 'utf8')
  const cWorkflow = load(cSource)
  const runnerWorkflow = load(runnerSource)

  // deploy-infra.yml calls both by `uses:`, so the reusable entrypoint and the inputs it passes
  // are contract, not prose. Read them off the parsed trigger — a commented-out `workflow_call:`
  // still satisfies a substring match while making every caller dead.
  assert.ok(cWorkflow.on.workflow_call, 'build-c.yml is no longer callable as a reusable workflow')
  assert.ok(cWorkflow.on.workflow_call.inputs.linux_x64_only, 'build-c.yml dropped the linux_x64_only input')
  assert.ok(cWorkflow.on.workflow_call.inputs.ref, 'build-c.yml dropped the ref input')
  assert.ok(runnerWorkflow.on.workflow_call, 'build-runner-binary.yml is no longer callable')
  assert.ok(runnerWorkflow.on.workflow_call.inputs.libboxlite_source, 'the C SDK source input is gone')
  assert.ok(runnerWorkflow.on.workflow_call.inputs.ref, 'build-runner-binary.yml dropped the ref input')

  // Each resolve-ref job's own checkout only sees THIS repo's branches (fetch-depth: 0), so its
  // "Validate build commit" step's cat-file check fails for a commit an open pull request (same
  // repo or fork) proposes, independently of whatever deploy-infra.yml already resolved — each
  // needs its own fetch-if-needed fallback. A bare-SHA `git fetch` is a real, working fallback
  // (confirmed live against the real GitHub server, fork heads included) — no PR number or ref
  // name required, unlike deploy-infra.yml's own resolve-ref, which needs a PR number for a
  // different reason (the security lookup, not fetchability). Anchored per line: a commented-out
  // fallback still parses.
  const cValidateRun = cWorkflow.jobs['resolve-ref'].steps.find((step: any) => step.name === 'Validate build commit')?.run
  assert.ok(cValidateRun, 'build-c.yml lost its Validate build commit step')
  assert.match(cValidateRun, /^\s*git cat-file -e "\$candidate\^\{commit\}" 2>\/dev\/null \|\| git fetch origin "\$candidate"/m)
  const runnerValidateRun = runnerWorkflow.jobs['resolve-ref'].steps.find(
    (step: any) => step.name === 'Validate build commit',
  )?.run
  assert.ok(runnerValidateRun, 'build-runner-binary.yml lost its Validate build commit step')
  assert.match(
    runnerValidateRun,
    /^\s*git cat-file -e "\$candidate\^\{commit\}" 2>\/dev\/null \|\| git fetch origin "\$candidate"/m,
  )

  // The upload/download names are the handshake between the two runs: build-c publishes
  // c-sdk-<target>, build-runner consumes c-sdk-linux-x64-gnu. Compare parsed values so a legal
  // requoting does not fail and a commented-out `name:` does not pass.
  const uploadName = (workflow: any, jobName: any) =>
    values(workflow.jobs)
      .flatMap((job) => job.steps ?? [])
      .find((step) => typeof step.uses === 'string' && step.uses.startsWith(jobName))?.with?.name
  assert.equal(uploadName(cWorkflow, 'actions/upload-artifact'), 'c-sdk-${{ matrix.target }}')
  assert.equal(uploadName(runnerWorkflow, 'actions/download-artifact'), 'c-sdk-linux-x64-gnu')
  // Scope to the `artifact)` case arm, not the file or even the step: build-runner-binary.yml
  // branches on libboxlite_source, and both arms set identity/archive — so a wider match still
  // passes when the commit-keyed names are moved under the `release)` label.
  const runnerStepRun = (name: any) =>
    values(runnerWorkflow.jobs)
      .flatMap((job) => job.steps ?? [])
      .find((step) => step.name === name)?.run
  const identityRun = runnerStepRun('Resolve artifact identity')
  assert.ok(identityRun, 'the artifact-identity step is missing')
  const commitArm =
    liveShell(identityRun)
      .split(/^\s*artifact\)\s*$/m)[1]
      ?.split(';;')[0] ?? ''
  assert.ok(commitArm, 'the commit-build case arm is missing')
  assert.match(commitArm, /identity="\$\{VERSION\}\+\$\{BUILD_SHA\}"/)
  assert.match(commitArm, /archive="boxlite-runner-v\$\{VERSION\}-\$\{BUILD_SHA\}-linux-amd64\.tar\.gz"/)
  assertShellLine(
    runnerStepRun('Build runner'),
    /github\.com\/boxlite-ai\/runner\/internal\.Version=\$\{VERSION_IDENTITY\}/,
  )

  // The extracted library is addressed, not hunted for. Searching /tmp walks root-owned siblings
  // (snap-private-tmp, systemd-private-*); find then reports those permission errors in its exit
  // status even when it matched, and `set -e` failed the step with the library already on disk.
  // build-c.yml packages one top-level directory named after the archive, which is the same
  // assumption the `release` branch of this step has always made.
  const extractRun = runnerStepRun('Extract commit libboxlite.a')
  assert.ok(extractRun, 'the commit libboxlite.a extraction step is missing')
  assertShellLine(extractRun, /cp "\/tmp\/\$\(basename "\$archive" \.tar\.gz\)\/lib\/libboxlite\.a" sdks\/go\/libboxlite\.a/)
  assert.doesNotMatch(liveShell(extractRun), /find \/tmp\b(?!\/c-sdk)/, 'the library must not be searched for under /tmp')
})

test('the cloud E2E suite is reachable only from a deploy or a human', () => {
  const workflow = load(readFileSync(E2E_CLOUD_WORKFLOW, 'utf8'))

  // The whole point of the trigger surface: this job builds and runs the tree in the same job
  // that holds a live dev API key, so no event may reach it that an outsider can raise. Compare
  // the parsed trigger keys as a set rather than asserting the two we want are present — the
  // failure to catch is a *re-added* `pull_request_target`, which every presence check passes.
  assert.deepEqual(Object.keys(workflow.on).sort(), ['workflow_call', 'workflow_dispatch'])

  // Read the callee's own declarations: a commented-out `workflow_call:` still satisfies a
  // substring match while making deploy-infra's call dead.
  assert.ok(workflow.on.workflow_call, 'e2e-cloud.yml is no longer callable as a reusable workflow')
  assert.equal(workflow.on.workflow_call.inputs.ref.required, true)
  assert.equal(workflow.on.workflow_call.secrets.BOXLITE_DEV_API_KEY.required, true)

  // A callee inherits the caller's token and may only narrow it, so this line — not anything in
  // deploy-infra.yml — is what keeps `id-token: write`, the deploy role's entry, away from a job
  // that executes the checked-out tree.
  assert.equal(workflow.permissions?.contents, 'read')
  assert.equal(workflow.permissions?.['id-token'], undefined)

  // The checkout has to follow the caller's commit. Falling back to github.sha alone would test
  // the tip of whatever ref the *caller* ran from, which for a re-deploy of an older commit is
  // not the commit now on the stack. actions/checkout's own ref:-driven fetch already resolves a
  // fork-derived commit — confirmed live against the real GitHub server — so no restructuring is
  // needed here, fork-derived deploys included.
  const checkout = workflow.jobs.e2e.steps.find(
    (step: any) => typeof step.uses === 'string' && step.uses.startsWith('actions/checkout'),
  )
  assert.ok(checkout, 'the e2e job no longer checks out a ref')
  assert.equal(checkout.with.ref, '${{ inputs.ref || github.sha }}')
})

test('the deploy role cannot reach another stage in the SST backing store', () => {
  // One bucket and one parameter tree hold every stage's state and secrets — the live bucket carries
  // secret/boxlite/dev.json beside secret/boxlite/prod.json. With s3:* and ssm:* on '*', a job bound to
  // the dev Environment could read prod's. This change makes the stage configuration authoritative in
  // that store, so it is the whole stack's config now, not just its app secrets.
  /*
   * A managed policy's contents live outside deploy-role-policy.json, so there is nothing here to
   * check it against — the deploy role must never carry one. bootstrap/aws.ts's ensureDeployRole is
   * the only place that grants anything to this role, so reading its own source for the one call
   * that could attach one is exhaustive, unlike a CloudFormation template where a managed policy
   * could be a resource anywhere in the file.
   */
  const ensureDeployRoleSource = readFileSync(join(REPO_ROOT, 'apps/infra/bootstrap/aws.ts'), 'utf8').match(
    /const ensureDeployRole = async[\s\S]*?\n}\n/,
  )?.[0]
  assert.ok(ensureDeployRoleSource, 'ensureDeployRole must exist in bootstrap/aws.ts')
  assert.doesNotMatch(
    ensureDeployRoleSource!,
    /attach-role-policy/,
    'the deploy role must never attach a managed policy; its grants must all be readable from deploy-role-policy.json',
  )

  const statements = deployRolePolicyStatements()
  assert.ok(statements.length > 0, 'the deploy role has no policy statements to check')
  const asArray = (value: any) => (Array.isArray(value) ? value : [value])

  // No statement may grant either service account-wide again.
  for (const statement of statements) {
    const actions = asArray(statement.Action)
    const wide = asArray(statement.Resource).includes('*')
    const storeReaching = actions.some((action: string) => /^(s3|ssm):/.test(action) && action.endsWith(':*'))
    assert.equal(
      wide && storeReaching,
      false,
      `${statement.Sid} grants ${actions.filter((a: string) => /^(s3|ssm):/.test(a)).join(', ')} on every resource`,
    )
  }

  // Every state object names the stage — the <STAGE> placeholder is visible as literal text before
  // aws.ts's render fills it in.
  const stateObjects = statements.find((statement: any) => statement.Sid === 'SstStateObjectsForThisStage')
  assert.ok(stateObjects, 'the stage-scoped state grant is missing')
  for (const resource of asArray(stateObjects.Resource)) {
    const scoped = resource.includes('<STAGE>') || resource.includes('sst-asset-')
    assert.ok(scoped, `${resource} is not scoped to the stage`)
  }

  // All seven prefixes SST writes. lock/ and summary/ are in this list precisely because a live
  // `list-objects` could not see them — a lock exists only while an update runs — so they came from
  // sst's own provider.go. A policy built from the listing alone fails when a deploy takes its lock.
  for (const prefix of ['app/', 'secret/', 'eventlog/', 'snapshot/', 'update/', 'lock/', 'summary/']) {
    assert.ok(
      asArray(stateObjects.Resource).some((resource: string) => resource.includes(`/${prefix}boxlite/`)),
      `no grant covers the ${prefix} state prefix, so a deploy cannot write it`,
    )
  }

  // `_fallback` belongs to the app, not a stage, so it cannot be scoped — it is read-only instead.
  const fallback = statements.find((statement: any) => statement.Sid === 'SstFallbackSecretsRead')
  assert.ok(fallback, 'the fallback secret grant is missing, so a stage using fallbacks cannot deploy')
  assert.deepEqual([...asArray(fallback.Action)].sort(), ['s3:GetObject', 's3:GetObjectVersion'])

  const parameters = statements.find((statement: any) => statement.Sid === 'SstParametersForThisStage')
  assert.ok(parameters, 'the stage-scoped parameter grant is missing')

  for (const statement of statements) {
    const mutating = sharedBootstrapMutations(statement)
    assert.deepEqual(mutating, [], `${statement.Sid} may not mutate the shared ${SHARED_BOOTSTRAP_PARAMETER}`)
  }
  const passphrase = asArray(parameters.Resource).find((resource: string) => resource.includes('/sst/passphrase/'))
  assert.ok(
    passphrase && passphrase.includes('<STAGE>'),
    'the passphrase decrypts one stage of state and must not be shared',
  )
})

test('release deployment consumes one published version for both components', () => {
  const source = readFileSync(RELEASE_DEPLOY_WORKFLOW, 'utf8')
  const workflow = load(source)
  const deployStep = workflow.jobs.deploy.steps.find(
    (step: any) => step.name === 'Deploy published API and Runner artifacts',
  )

  // Read the parsed job env, not the file: these three must be on the *job* for the deploy step
  // to inherit them. Moved onto any single step they still match a substring search, while the
  // deploy silently falls back to COMPONENT_DEFAULT_KINDS and rebuilds the API from the checkout.
  for (const selector of ['BOXLITE_ARTIFACT_SOURCE', 'API_ARTIFACT_SOURCE', 'RUNNER_ARTIFACT_SOURCE']) {
    assert.equal(workflow.jobs.deploy.env[selector], 'release', `${selector} must be set on the deploy job`)
  }
  // Off unless asked for: the API has no downgrade guard, so an older VERSION deployed without
  // this moves the API back while the Runner refuses, and the workflow still reports success.
  // The unanchored source match would have accepted `default: true` plus any later false.
  assert.equal(workflow.on.workflow_dispatch.inputs.allow_downgrade.default, false)
  assert.equal(workflow.on.workflow_dispatch.inputs.allow_downgrade.type, 'boolean')
  // These two carry the inputs into the deploy job; commented out, the defaults above become
  // decoration. Read the parsed job env rather than the file.
  assert.equal(workflow.jobs.deploy.env.ALLOW_DOWNGRADE, "${{ inputs.allow_downgrade && '1' || '' }}")
  assert.equal(workflow.jobs.deploy.env.VERSION, '${{ inputs.version }}')
  assert.match(source, /environment: \$\{\{ inputs\.stage \}\}/)
  // `stage` picks the protected Environment holding the AWS role, so it must be untypable
  // rather than merely wrong — the same allowlist rule deploy-infra.yml follows.
  assert.equal(workflow.on.workflow_dispatch.inputs.stage.type, 'choice')
  assert.deepEqual(workflow.on.workflow_dispatch.inputs.stage.options, ['dev', 'prod'])
  assert.ok(deployStep)
  assertNoMaterializedStageConfig(workflow, source)
  assertComposedDeployRoleArn(source, '${{ inputs.stage }}')
  // The same guarded wrapper and the same pre-deploy gates the build path uses: a release
  // deploy reconciles the identical stack, so it owes the identical safety checks.
  assert.equal(deployStep.shell, 'bash')
  // Through the guarded wrapper (`npm run deploy`, never the sst binary) and with the mandatory
  // Runner policy — a release reconciles the identical stack, so it owes the identical guards. A
  // release deploy takes no --exclude: it is always the whole stack at one published version.
  const releaseDeployShell = liveShell(deployStep.run)
  assert.match(releaseDeployShell, /npm run deploy -- --stage "\$STAGE" --policy policies\/runner/)
  assert.doesNotMatch(releaseDeployShell, /--exclude/)
  assert.ok(workflow.jobs.deploy.steps.find((step: any) => step.name === 'Run deployment safety tests'))
  const boundaryStep = workflow.jobs.deploy.steps.find(
    (step: any) => step.name === 'Verify deploy role IAM boundary permissions',
  )
  assert.ok(boundaryStep, 'the release deploy skips the IAM boundary preflight')
  assert.ok(
    workflow.jobs.deploy.steps.indexOf(boundaryStep) < workflow.jobs.deploy.steps.indexOf(deployStep),
    'the boundary preflight must run before the deploy it protects',
  )
})

test('the deployment workflows cap the token they hand their jobs', () => {
  // CodeQL actions/missing-workflow-permissions: without a top-level block a job inherits the
  // repository default, which can be write. Scoped to the workflows this change owns — ten
  // others in the directory predate it and are not this commit's to re-scope.
  for (const entry of [
    'build-apps-api-image.yml',
    'build-c.yml',
    'build-runner-binary.yml',
    'deploy-infra.yml',
    'deploy-release.yml',
    // The deploy path's third reusable callee. Its cap does more work than the others': it is
    // what narrows the inherited deploy token, so it belongs under the same guard.
    'e2e-cloud.yml',
  ]) {
    const workflow = load(readFileSync(join(REPO_ROOT, '.github/workflows', entry), 'utf8'))
    assert.equal(workflow.permissions?.contents, 'read', `${entry} must default its token to contents: read`)

    // A job may raise its own, but only deliberately, and only the scope it has a reason for —
    // by scope rather than by job, so a job that already has one reason to raise cannot pick up
    // an unrelated second one unnoticed:
    //   upload-to-release — actually writes (uploads release assets)
    //   build-c / build-runner — write nothing; they call a workflow whose release-upload job
    //     declares contents: write (see the caller-grant test below). Granting per job rather
    //     than at the top keeps `deploy` at contents: read.
    //   build-api — calls a workflow that assumes an AWS role through OIDC. A job-level block
    //     replaces the workflow-level one, so it restates id-token rather than inheriting it.
    const expectedWriters: Record<string, string[]> = {
      'upload-to-release': ['contents'],
      'build-c': ['contents'],
      'build-runner': ['contents'],
      'build-api': ['id-token'],
    }
    for (const [jobName, job] of entries(workflow.jobs)) {
      if (!job.permissions) continue
      const raised = entries(job.permissions)
        .filter(([, level]) => level === 'write')
        .map(([scope]) => scope)
      const allowed = expectedWriters[jobName] ?? []
      const unexpected = raised.filter((scope) => !allowed.includes(scope))
      assert.deepEqual(unexpected, [], `${entry} job '${jobName}' raises ${JSON.stringify(unexpected)} without a reason`)
    }
  }
})

test('a job calling a reusable workflow grants at least what that workflow asks for', () => {
  // Documented: "permissions can only be maintained or reduced—not elevated—throughout the
  // chain" — https://docs.github.com/en/actions/how-tos/reuse-automations/reuse-workflows
  //
  // Measured: dispatching deploy-infra at 32cfa5c3, where the build-c job granted less than
  // build-c.yml asks for, produced a startup_failure with zero jobs, zero logs and zero
  // annotations — GitHub said only "a workflow file issue". actionlint exits 0 on that file.
  //
  // Inferred: that the elevation is what rejected the run. The docs describe the ceiling, not
  // what happens when a callee asks past it, and GitHub never named a cause. If a dispatch
  // shows the callee simply running with a reduced token instead, revisit this comment.
  const directory = join(REPO_ROOT, '.github/workflows')
  const LOCAL = './.github/workflows/'
  const RANK: Record<string, number> = { none: 0, read: 1, write: 2 }
  const readWorkflow = (file: any) => load(readFileSync(join(directory, file), 'utf8'))

  // The widest permission any job in the callee asks for, following nested calls.
  const required = (file: any, seen = new Set<string>()): Record<string, string> => {
    if (seen.has(file)) return {}
    seen.add(file)
    const workflow = readWorkflow(file)
    const widest: Record<string, string> = {}
    const merge = (permissions: any) => {
      if (!permissions || typeof permissions !== 'object') return
      for (const [scope, level] of entries(permissions)) {
        if ((RANK[level] ?? 0) > (RANK[widest[scope]] ?? 0)) widest[scope] = level
      }
    }
    merge(workflow.permissions)
    for (const job of values(workflow.jobs)) {
      merge(job.permissions)
      if (typeof job.uses === 'string' && job.uses.startsWith(LOCAL)) {
        merge(required(job.uses.slice(LOCAL.length), seen))
      }
    }
    return widest
  }

  let checked = 0
  for (const entry of readdirSync(directory).filter((file) => /\.ya?ml$/.test(file))) {
    const workflow = readWorkflow(entry)
    for (const [jobName, job] of entries(workflow.jobs)) {
      if (typeof job.uses !== 'string' || !job.uses.startsWith(LOCAL)) continue
      // A job-level block replaces the workflow-level one rather than merging with it.
      const granted: Record<string, string> = job.permissions ?? workflow.permissions ?? {}
      for (const [scope, level] of entries(required(job.uses.slice(LOCAL.length)))) {
        assert.ok(
          (RANK[granted[scope]] ?? 0) >= (RANK[level] ?? 0),
          `${entry} job '${jobName}' grants ${scope}: ${granted[scope] ?? 'none'} but ` +
            `${job.uses.slice(LOCAL.length)} needs ${level}`,
        )
      }
      checked += 1
    }
  }
  assert.ok(checked >= 11, `expected every local reusable call to be swept, saw ${checked}`)
})

test('every workflow that selects a deployment Environment does so from an allowlist', () => {
  // The rule is stated once in .github/workflows/README.md and enforced here across every
  // workflow file, so a fourth deploy workflow cannot quietly reintroduce a free-text stage that
  // reaches a required-reviewers Environment through a typo. Read `environment` off the parsed
  // job rather than matching source text: GitHub accepts both the bare string and the
  // `{ name, url }` object, and the object form is what you write to surface a deployment URL —
  // so a text matcher would miss exactly the workflows most likely to use it.
  const workflowDirectory = join(REPO_ROOT, '.github/workflows')
  const environmentInput = /^\$\{\{\s*(?:github\.event\.)?inputs\.([A-Za-z_][A-Za-z0-9_-]*)\s*(?:\|\||\}\})/
  const swept = new Set()

  for (const entry of readdirSync(workflowDirectory)) {
    if (!/\.ya?ml$/.test(entry)) continue
    const workflow = load(readFileSync(join(workflowDirectory, entry), 'utf8'))
    const inputs = workflow.on?.workflow_dispatch?.inputs ?? {}

    for (const [jobName, job] of entries(workflow.jobs)) {
      const environment = typeof job.environment === 'string' ? job.environment : job.environment?.name
      if (!environment) continue
      const where = `${entry} job '${jobName}'`

      // Fail closed on any indirection. `needs.*.outputs.*`, `env.*`, and `vars.*` all reach an
      // Environment through a value this test cannot follow, so requiring the direct input is
      // the only form that stays checkable — an indirect one must be made explicit here first.
      const selected = environment.match(environmentInput)
      assert.ok(
        selected || !environment.includes('${{'),
        `${where} selects an Environment through an expression this guard cannot follow: ${environment}`,
      )
      if (selected) {
        const declared = inputs[selected[1]]
        assert.ok(declared, `${where} selects an Environment from an undeclared input '${selected[1]}'`)
        assert.equal(declared.type, 'choice', `${where} input '${selected[1]}' must be an allowlist`)
        assert.ok(declared.options?.length > 0, `${where} input '${selected[1]}' has an empty allowlist`)
      }

      // The same job that reaches a protected Environment reaches the AWS role behind it, so
      // the main-only rule is asserted here rather than as a bare substring per workflow.
      assert.match(job.if ?? '', /github\.ref == 'refs\/heads\/main'/, `${where} is not restricted to main`)
      swept.add(entry)
    }
  }

  assert.deepEqual(
    [...swept].sort(),
    // mbuild.yml and mdeploy.yml are the mstage/mbuild/mdeploy replacements,
    // dispatched by hand while the incumbents above still run. Listed here
    // deliberately: the point of pinning the set is that a fifth deploy
    // workflow is a reviewed addition rather than one that appeared.
    ['build-apps-api-image.yml', 'deploy-infra.yml', 'deploy-release.yml', 'mbuild.yml', 'mdeploy.yml'],
    'the swept set no longer matches the deployment workflows',
  )
})

test('API publishing builds once and promotes that exact image without rebuilding', () => {
  const source = readFileSync(API_IMAGE_BUILD_WORKFLOW, 'utf8')
  const workflow = load(source)

  for (const input of ['stage', 'source_stage']) {
    assert.equal(workflow.on.workflow_dispatch.inputs[input].type, 'choice', `${input} must be an allowlist`)
    assert.deepEqual(workflow.on.workflow_dispatch.inputs[input].options, ['dev', 'prod'])
  }

  // A release event runs on a tag ref, which the branch-scoped deployment Environments block
  // before the job can reach its AWS role — so publishing is dispatched from main instead.
  // Read the parsed trigger: `on:` and `"on":` are the same key, and only one is greppable.
  assert.equal(workflow.on.release, undefined, 'publishing must not trigger on a release event')
  assert.deepEqual(Object.keys(workflow.on), ['workflow_call', 'workflow_dispatch'])

  // Commit mode: how deploy-infra.yml builds the Api for the commit it deploys. `ref` is what
  // selects it, so it has to be call-only — a dispatch that could set it would let someone tag an
  // image for a commit the deployable-commit guard never saw.
  assert.deepEqual(Object.keys(workflow.on.workflow_call.inputs).sort(), ['ref', 'stage'])
  assert.equal(workflow.on.workflow_dispatch.inputs.ref, undefined)
  assert.equal(workflow.on.workflow_call.inputs.ref.required, true)
  assert.equal(workflow.on.workflow_call.inputs.stage.required, true)
  const commitCheckout = workflow.jobs.publish.steps.find((step: any) => step.name === 'Checkout the selected commit')
  assert.ok(commitCheckout, 'the commit checkout step is missing')
  assert.equal(commitCheckout.with.ref, '${{ inputs.ref }}')
  // The build must compile the release tag, not whatever main points at now — read it off the
  // parsed step, since a commented-out `ref:` still satisfies a substring match.
  const releaseCheckout = workflow.jobs.publish.steps.find((step: any) => step.name === 'Checkout the released tag')
  assert.ok(releaseCheckout, 'the released-tag checkout step is missing')
  assert.equal(releaseCheckout.with.ref, 'refs/tags/v${{ inputs.version }}')
  // This workflow compiles the released image, so it owes the same native-arch guard the deploy
  // path has — a second copy of the step means pinning it in deploy-infra.yml says nothing here.
  const publishArch = workflow.jobs.publish.steps.find((step: any) => step.name === 'Verify native AMD64 Docker')
  assert.ok(publishArch, 'the native-arch guard step is missing')
  assertShellLine(publishArch.run, /test "\$\(uname -m\)" = "x86_64"/)
  assertShellLine(publishArch.run, /test "\$\(docker info --format '\{\{\.Architecture\}\}'\)" = "x86_64"/)

  const resolveRun = workflow.jobs.publish.steps.find((step: any) => step.name === 'Resolve publish operation')?.run ?? ''
  assertShellLine(resolveRun, /tag v\$version declares Cargo\.toml version/)
  assertShellLine(resolveRun, /builds always land in dev/)
  // The one string the deploy has to agree with. apiImageTag() in artifacts/api.ts derives the
  // reference SST is handed; if these two drift the deploy resolves a tag nothing ever pushed and
  // only finds out when the ECS task fails to pull. artifacts/api.test.ts pins the other half.
  assertShellLine(resolveRun, /tag="v\$\{version\}-\$\{INPUT_REF\}"/)
  // A ref that is not a full lowercase sha would tag an image nobody can address again.
  assertShellLine(resolveRun, /\[\[ "\$INPUT_REF" =~ \^\[0-9a-f\]\{40\}\$ \]\]/)
  // "builds once and promotes that exact image" is a property of *which step* compiles. Matched
  // against the whole file, moving `docker build` into the promote step reads as unchanged.
  const stepRun = (name: any) => workflow.jobs.publish.steps.find((step: any) => step.name === name)?.run ?? ''
  const buildRun = stepRun('Build the image once')
  const promoteRun = stepRun('Promote the exact published image')
  assertShellLine(buildRun, /docker build --file apps\/api\/Dockerfile/)
  // A registry-side manifest copy, not a daemon round-trip: pull + tag + push re-uploads the
  // image and can change its digest, which is exactly what "promotes that exact image" denies.
  // By digest, not by tag: the source tag can move between the check above and this copy, and a
  // comparison afterwards would only notice once the wrong image was already published.
  assertShellLine(promoteRun, /docker buildx imagetools create --prefer-index=false/)
  assertShellLine(promoteRun, /--tag "\$TARGET:\$TAG" "\$SOURCE@\$SOURCE_DIGEST"/)
  // Over the live shell: the comment above the copy names the `docker build`/`docker push` pair
  // it exists to explain, and a mention in a comment is not a rebuild. Both intervening tokens
  // are spelled out rather than excluded by a trailing space — the step already invokes `docker
  // buildx`, and `docker image push|tag` is the management-command form of the same verbs, so
  // either is a plausible way a rebuild returns. `imagetools create` stays exempt because it is
  // none of build/pull/push/tag (and `\b` keeps `build` from matching inside `buildx`).
  assert.doesNotMatch(
    liveShell(promoteRun),
    /docker (?:buildx |image )?(?:build|pull|push|tag)\b/,
    'promotion must copy, never rebuild or re-upload',
  )
  // And it proves preservation rather than assuming it.
  assertShellLine(promoteRun, /if \[ "\$promoted" != "\$SOURCE_DIGEST" \]; then/)
})

test('promotion reads the source stage in the source stage\'s own region', () => {
  // The defect this guards, observed promoting 0.10.0 from dev to prod: an ECR registry is one
  // account in ONE region, and `vars` resolves against the single Environment a job binds to —
  // the target's. So every source lookup ran in the target's region. With dev in ap-southeast-1
  // and prod in us-west-2 that read boxlite-app-dev-api out of us-west-2, where it does not
  // exist, and the guard reported the image as never published while it sat in the other region.
  // The IAM side already spans regions (ecr:* on Resource '*'), so region is the whole of it.
  const workflow = load(readFileSync(API_IMAGE_BUILD_WORKFLOW, 'utf8')) as any
  const job = workflow.jobs.publish
  const stepRun = (name: string) => job.steps.find((step: any) => step.name === name)?.run ?? ''

  // Dispatch-only, like every other promote knob. The existing workflow_call assertion above
  // pins that side by enumerating its inputs, so adding this one there fails there.
  assert.equal(workflow.on.workflow_dispatch.inputs.source_region.required, false)
  assert.equal(workflow.on.workflow_dispatch.inputs.source_region.default, '')

  // Unset falls back to the target's region, which is what a same-region promote wants and what
  // the workflow did unconditionally before. The order matters: the input has to win.
  assert.equal(job.env.SOURCE_REGION, "${{ inputs.source_region || vars.AWS_REGION || 'ap-southeast-1' }}")
  assert.equal(job.env.AWS_REGION, "${{ vars.AWS_REGION || 'ap-southeast-1' }}")

  // A docker credential is per registry host, so the copy needs its own login when the source is
  // elsewhere — the AWS-side grant does not carry docker.
  const loginRun = stepRun('Log in to ECR')
  assertShellLine(loginRun, /source_registry="\$\{account\}\.dkr\.ecr\.\$\{SOURCE_REGION\}\.amazonaws\.com"/)
  assertShellLine(loginRun, /target_registry="\$\{account\}\.dkr\.ecr\.\$\{AWS_REGION\}\.amazonaws\.com"/)
  assertShellLine(loginRun, /if \[ "\$source_registry" != "\$target_registry" \]; then/)
  assertShellLine(loginRun, /get-login-password --region "\$SOURCE_REGION"/)
  // $SOURCE is what the copy addresses; built from the target's registry it points at the wrong
  // region however correct the digest is.
  assertShellLine(loginRun, /echo "source=\$\{source_registry\}\/boxlite-app-\$\{SOURCE_STAGE\}-api"/)

  const verifyRun = stepRun('Verify bootstrap repositories and target tag')
  // The source read moves; the two target reads must not.
  assertShellLine(verifyRun, /describe-images --region "\$SOURCE_REGION" --repository-name "\$source_name"/)
  assertShellLine(verifyRun, /describe-repositories --region "\$AWS_REGION" --repository-names "\$target_name"/)
  assertShellLine(verifyRun, /describe-images --region "\$AWS_REGION" --repository-name "\$target_name"/)
  assert.doesNotMatch(
    liveShell(verifyRun),
    /describe-images --region "\$AWS_REGION" --repository-name "\$source_name"/,
    'the source repository must never be read in the target region',
  )
  // Region in both refusals: "has not been published" with no location is what sent the operator
  // looking for an unbuilt image instead of a misaddressed one.
  assertShellLine(verifyRun, /was not found in \$SOURCE_REGION/)
  assertShellLine(verifyRun, /has not been published in \$SOURCE_REGION/)
  // `--query` on a present repository with an absent tag prints None and exits zero, so the `||`
  // branch cannot catch it and the digest reaches the copy as "$SOURCE@None". Same hole as
  // artifacts/api.ts:104, which is why the deploy side already guards it.
  assertShellLine(verifyRun, /if \[ -z "\$source_digest" \] \|\| \[ "\$source_digest" = None \]; then/)
})

test('infrastructure tests cannot persist or write with the workflow token', () => {
  const source = readFileSync(LINT_WORKFLOW, 'utf8')
  const infraJobStart = source.indexOf('\n  infra:\n')
  const infraJobEnd = source.indexOf('  # Single required status check', infraJobStart)
  assert.notEqual(infraJobStart, -1, 'infra job marker is missing from lint.yml')
  assert.notEqual(infraJobEnd, -1, 'required-status marker is missing from lint.yml')
  const infraJob = source.slice(infraJobStart, infraJobEnd)

  assert.match(infraJob, /permissions:\s+contents: read/)
  assert.match(infraJob, /uses: actions\/checkout@v5\s+with:\s+persist-credentials: false/)
})

test('dev deploy role trusts only the repository GitHub Environment identity', () => {
  assert.ok(existsSync(DEV_DEPLOY_ROLE_TRUST), 'the deploy role trust document is missing')
  const trust = readFileSync(DEV_DEPLOY_ROLE_TRUST, 'utf8')
  const policy = readFileSync(DEV_DEPLOY_ROLE_POLICY, 'utf8')
  const awsSource = readFileSync(join(REPO_ROOT, 'apps/infra/bootstrap/aws.ts'), 'utf8')
  const statements = readRuntimeBoundaryStatements()

  assert.match(trust, /oidc-provider\/token\.actions\.githubusercontent\.com/)
  assert.match(trust, /token\.actions\.githubusercontent\.com:aud.{0,20}sts\.amazonaws\.com/s)
  assert.match(trust, /token\.actions\.githubusercontent\.com:sub.{0,20}repo:<REPO>:environment:<STAGE>/s)
  assert.doesNotMatch(trust, /AdministratorAccess/)
  assert.doesNotMatch(policy, /AdministratorAccess/)
  assert.match(policy, /policy\/boxlite-<STAGE>-runtime-boundary/)
  assert.match(policy, /iam:PermissionsBoundary/)
  assert.match(awsSource, /'boxlite-sst-deploy'/)

  assert.deepEqual(findStatement(statements, 'BoxLiteStageSecrets'), {
    Sid: 'BoxLiteStageSecrets',
    Effect: 'Allow',
    Action: ['secretsmanager:DescribeSecret', 'secretsmanager:GetSecretValue'],
    Resource: 'arn:aws:secretsmanager:<REGION>:<ACCOUNT>:secret:boxlite-<STAGE>-*',
  })
  assert.deepEqual(findStatement(statements, 'BoxLiteStageKmsKeys'), {
    Sid: 'BoxLiteStageKmsKeys',
    Effect: 'Allow',
    Action: 'kms:Decrypt',
    Resource: 'arn:aws:kms:<REGION>:<ACCOUNT>:key/*',
    Condition: {
      'ForAnyValue:StringLike': {
        'kms:ResourceAliases': 'alias/boxlite-<STAGE>-*',
      },
    },
  })
  assert.deepEqual(findStatement(statements, 'BoxLiteBuckets'), {
    Sid: 'BoxLiteBuckets',
    Effect: 'Allow',
    Action: [
      's3:CreateBucket',
      's3:DeleteBucket',
      's3:GetBucketLocation',
      's3:ListBucket',
      's3:ListBucketVersions',
      's3:PutBucketTagging',
    ],
    Resource: ['arn:aws:s3:::boxlite-<STAGE>-*', 'arn:aws:s3:::boxlite-app-<STAGE>-*', 'arn:aws:s3:::boxlite-volume-*'],
  })
  assert.deepEqual(findStatement(statements, 'BoxLiteBucketObjects'), {
    Sid: 'BoxLiteBucketObjects',
    Effect: 'Allow',
    Action: ['s3:AbortMultipartUpload', 's3:DeleteObject', 's3:DeleteObjectVersion', 's3:GetObject', 's3:PutObject'],
    Resource: [
      'arn:aws:s3:::boxlite-<STAGE>-*/*',
      'arn:aws:s3:::boxlite-app-<STAGE>-*/*',
      'arn:aws:s3:::boxlite-volume-*/*',
    ],
  })
  /*
   * The last sibling, and the one the unscoped-grant test cannot reach: it collects the deploy role's
   * own policy, and this statement lives in the boundary policy, which names no role. So `boxlite-*`
   * here — a task assuming another stage's runtime role — was revertible with every test still green,
   * which is the failure shared/resource-name.ts:22-25 already records once.
   */
  assert.deepEqual(findStatement(statements, 'AssumeBoxLiteRuntimeRoles'), {
    Sid: 'AssumeBoxLiteRuntimeRoles',
    Effect: 'Allow',
    Action: 'sts:AssumeRole',
    Resource: 'arn:aws:iam::<ACCOUNT>:role/boxlite-<STAGE>-*',
  })
})

test('the stage bootstrap owns artifact stores needed before an SST deploy can start', () => {
  // No declarative resource tree to deepEqual any more — aws.ts creates these imperatively. This
  // reads its source for the specific flags a CloudFormation Properties block used to carry
  // declaratively, the same way other tests in this file pin a stack/*.ts source rather than
  // executing it. bootstrap/aws.test.ts exercises the actual reconcile behavior end to end.
  const awsSource = readFileSync(join(REPO_ROOT, 'apps/infra/bootstrap/aws.ts'), 'utf8')

  // One spelling: aws.ts calls the same composers artifacts/api.ts and artifacts/runner.ts read
  // the names back with, rather than restating boxlite-app-<stage>-api/-artifacts-<account> here.
  assert.match(awsSource, /apiImageRepository\(\{ app: 'boxlite', stage \}\)/)
  assert.match(awsSource, /runnerArtifactsBucketName\(\{ app: 'boxlite', stage, accountId \}\)/)

  assert.match(awsSource, /'--image-tag-mutability',\s*\n\s*'IMMUTABLE'/)
  assert.match(awsSource, /'--image-scanning-configuration',\s*\n\s*'scanOnPush=true'/)
  assert.match(awsSource, /'--encryption-configuration',\s*\n\s*'encryptionType=AES256'/)
  // Never deleted or replaced — a stronger guarantee than CloudFormation's DeletionPolicy: Retain
  // gave the same two resources.
  assert.doesNotMatch(awsSource, /'delete-repository'|'delete-bucket'/)

  assert.match(awsSource, /ApplyServerSideEncryptionByDefault.*SSEAlgorithm.*AES256/)
  assert.match(awsSource, /'--versioning-configuration',\s*\n\s*'Status=Enabled'/)
  for (const flag of ['BlockPublicAcls=true', 'IgnorePublicAcls=true', 'BlockPublicPolicy=true', 'RestrictPublicBuckets=true']) {
    assert.ok(awsSource.includes(flag), `missing public-access-block flag: ${flag}`)
  }
  assert.match(awsSource, /ID:\s*'expire-superseded-runner-builds'/)
  assert.match(awsSource, /Prefix:\s*'runner\/'/)
  assert.match(awsSource, /NoncurrentVersionExpiration:\s*\{\s*NoncurrentDays:\s*30\s*\}/)
})

test('one release selector resolves the API and Runner to the same published version', () => {
  const source = readFileSync(SST_WRAPPER, 'utf8')

  assert.match(source, /const workspaceVersion = readWorkspaceVersion\(\)/)
  assert.match(source, /resolvePublicDeploymentConfig\(process\.env, workspaceVersion\)/)
  // resolveArtifactSource uses the same resolveReleaseVersion(workspace, env) contract as the
  // public deployment config. VERSION therefore selects both artifacts instead of producing an
  // API/Runner split-brain release.
  assert.match(source, /resolveArtifactSource\('runner'\)/)
  assert.match(source, /await verifyRunnerArtifact\(runnerSource/)
  assert.doesNotMatch(source, /verifyRunnerReleaseAssets\(publicDeploymentConfig\.releaseVersion\)/)
})

test('every hand-written spelling of an artifact name agrees with the composer', () => {
  // The defect this guards: the name is resolved in JS and written by hand in the workflow that
  // produces the artifact. bootstrap/aws.ts creates the two resources by calling the same
  // composer (pinned in the previous test), so there is no longer a second declared spelling to
  // drift from it — only the hand-written bash below can still disagree. Renaming the composer's
  // grammar and not the workflow leaves a bootstrap that creates boxlite-app-dev-api and a build
  // that pushes to boxlite-dev-api — the workflow's own "repository is missing" guard fires and
  // the deploy fails, with every unit test still green. Bash cannot import awsResourceName, so
  // agreement is asserted here instead.
  const apiWorkflow = readFileSync(API_IMAGE_BUILD_WORKFLOW, 'utf8')
  const deployWorkflow = readFileSync(DEPLOY_WORKFLOW, 'utf8')

  assert.equal(apiImageRepository({ app: 'boxlite', stage: 'dev' }), 'boxlite-app-dev-api')
  assert.equal(
    runnerArtifactsBucketName({ app: 'boxlite', stage: 'dev', accountId: '123456789012' }),
    'boxlite-app-dev-artifacts-123456789012',
  )

  // Written by hand, in the two producers. Anchored per line so a commented-out spelling cannot
  // satisfy the match, and the old shape is refused outright rather than merely not found.
  assertShellLine(apiWorkflow, /boxlite-app-\$\{TARGET_STAGE\}-api/)
  assertShellLine(apiWorkflow, /boxlite-app-\$\{SOURCE_STAGE\}-api/)
  assertShellLine(deployWorkflow, /bucket="boxlite-app-\$\{STAGE\}-artifacts-\$\{account_id\}"/)
  assert.doesNotMatch(liveShell(apiWorkflow), /boxlite-\$\{(TARGET|SOURCE)_STAGE\}-api/)
  assert.doesNotMatch(liveShell(deployWorkflow), /boxlite-\$\{STAGE\}-artifacts/)
})

test('the runtime boundary admits the bucket the Runner is actually pointed at', () => {
  // The boundary intersects with every identity policy SST writes, so a bucket outside its
  // prefixes is denied however generous the grant. Renaming the bucket without widening the
  // boundary denied the Runner its own binary — at boot and on every SSM upgrade — while every
  // other test stayed green, because CI pushes with the deploy role's s3:* rather than the
  // instance profile. Derive the name, do not spell it.
  const bucket = runnerArtifactsBucketName({ app: 'boxlite', stage: 'dev', accountId: '123456789012' })
  const prefixes = findStatement(readRuntimeBoundaryStatements(), 'BoxLiteBucketObjects').Resource.map((arn: any) =>
    arn.replace('arn:aws:s3:::', '').replace('<STAGE>', 'dev'),
  )
  const admits = prefixes.some((pattern: any) => new RegExp(`^${pattern.replace(/\*/g, '.*')}$`).test(`${bucket}/runner/x`))
  assert.ok(admits, `no BoxLiteBucketObjects prefix admits ${bucket}/runner/*; prefixes: ${prefixes.join(', ')}`)
})
