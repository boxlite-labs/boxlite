// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

/*
 * The AWS half of `npm run bootstrap -- --stage <stage>`: the GitHub OIDC
 * deploy role, its runtime IAM permissions boundary, the Api image repository
 * and the Runner artifacts bucket — everything `mdeploy`/`mbuild` need on AWS
 * and cannot create for themselves, for the same shape of reason gcp.ts's own
 * header gives for its half: the deploy role cannot create itself, every role
 * SST creates must carry the runtime boundary before the first apply
 * (stack/deploy.ts's `$transform`), CI has to push an Api image before any
 * release/build deploy starts, and a build-mode Runner install reads a
 * tarball CI staged before the deploy that installs it ran.
 *
 * Reconciles rather than creates, exactly like gcp.ts: re-running is how an
 * edit to one of the JSON documents in aws/ reaches AWS. That replaces what
 * `aws cloudformation deploy` against bootstrap/aws/github-deploy-role.yaml
 * used to do, with the same shape boxlite-backoffice and boxlite-commerce use
 * for their own AWS bootstrap (apps/infra/iam/*.json there): checked-in IAM
 * documents, applied by a reconcile script instead of a CloudFormation stack.
 * See aws/README.md for what each document grants and why.
 *
 * Talks to the AWS CLI rather than adding an SDK client this directory does
 * not otherwise carry — mbuild already reaches ECR the same way
 * (mbuild/src/publish.ts), and the CLI is a prerequisite for deploying at all.
 */

import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

import { apiImageRepository } from '../artifacts/api.js'
import { runnerArtifactsBucketName } from '../artifacts/runner.js'
import { runtimeBoundaryPolicyArn } from '../deployment/environment.js'
import { githubDeployRoleName } from './environment.js'

export class AwsBootstrapError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'AwsBootstrapError'
  }
}

/** One external command. A non-zero exit is reported, never thrown away. */
export type RunResult = { code: number; stdout: string; stderr: string }
export type RunOptions = { stdin?: string }
export type Run = (command: string, args: string[], options?: RunOptions) => Promise<RunResult>

/** The documents live beside this file; this module is an implementation of them. */
const DOCUMENTS = fileURLToPath(new URL('./aws', import.meta.url))

// Matches the CFN template's MaxSessionDuration. A soak deploy needs the full two hours; an
// ordinary one finishes in a fraction of it, so there is no cost to granting it every time.
const DEPLOY_SESSION_DURATION_SECONDS = '7200'

// A managed policy caps at 5 versions; create-policy-version fails LimitExceeded on the 6th
// unless an old, non-default one is pruned first. See ensureRuntimeBoundary.
const MANAGED_POLICY_VERSION_LIMIT = 5

type DocumentScope = { repo: string; stage: string; accountId: string; region: string }

/**
 * Renders one of the JSON documents in `aws/` for one repository, stage,
 * account and region.
 *
 * The account and region are placeholders for the same reason the stage is: a
 * literal in the file is a second declaration to keep in step with the
 * credentials actually creating the role, and disagreement fails as a grant
 * made in the wrong place rather than as a visible error.
 *
 * Exported because the useful assertion is that a real document comes out of
 * this with nothing left to substitute.
 */
export const render = (contents: string, { repo, stage, accountId, region }: DocumentScope): string =>
  contents.replaceAll('<REPO>', repo).replaceAll('<STAGE>', stage).replaceAll('<ACCOUNT>', accountId).replaceAll('<REGION>', region)

const documentFor = (file: string, scope: DocumentScope): string => {
  let contents: string
  try {
    contents = readFileSync(join(DOCUMENTS, file), 'utf8')
  } catch (error) {
    throw new AwsBootstrapError(`Could not read bootstrap/aws/${file}: ${(error as Error).message}`)
  }
  return render(contents, scope)
}

/**
 * The AWS calls this makes, split by what a failure means.
 *
 * `--region` is passed to IAM as well as ECR/S3. IAM is global and ignores it;
 * one helper that always passes it is less surprising than one that does and
 * one that doesn't, drifting apart later.
 */
type Aws = {
  /** Whether a resource is already there. Absence is an answer, not a failure. */
  present: (args: string[]) => Promise<boolean>
  /** A question with an answer worth reading, parsed from the CLI's `--output json`. */
  read: (what: string, args: string[]) => Promise<any>
  /** A change. A non-zero exit fails the run, carrying the CLI's own message. */
  apply: (what: string, args: string[]) => Promise<void>
}

const awsFor = ({ run, region }: { run: Run; region: string }): Aws => {
  const call = (args: string[]): Promise<RunResult> => run('aws', [...args, '--region', region])
  return {
    async present(args) {
      return (await call(args)).code === 0
    },
    async read(what, args) {
      const result = await call([...args, '--output', 'json'])
      if (result.code !== 0) {
        throw new AwsBootstrapError(`${what}: ${result.stderr.trim() || `aws exited ${result.code}`}`)
      }
      try {
        return JSON.parse(result.stdout)
      } catch {
        throw new AwsBootstrapError(`${what}: aws returned invalid JSON (${result.stdout.slice(0, 200)})`)
      }
    },
    async apply(what, args) {
      const result = await call(args)
      if (result.code === 0) return
      throw new AwsBootstrapError(`${what}: ${result.stderr.trim() || `aws exited ${result.code}`}`)
    },
  }
}

/**
 * The per-stage runtime permissions boundary every role SST creates must
 * carry. A customer-managed policy, not a role, so updating it is a new
 * policy *version* rather than an overwrite — see MANAGED_POLICY_VERSION_LIMIT.
 * Not a concern backoffice/commerce's own bootstrap has: both assume one
 * boundary already exists account-wide and never touch its lifecycle. Mine's
 * is per-stage (runtimeBoundaryPolicyArn) because this stack provisions its
 * own roles from nothing and needs a boundary before the first one exists, so
 * this file owns creating and versioning it.
 */
const ensureRuntimeBoundary = async ({ aws, scope, log }: { aws: Aws; scope: DocumentScope; log: (line: string) => void }): Promise<string> => {
  const arn = runtimeBoundaryPolicyArn({ accountId: scope.accountId, appName: 'boxlite', stage: scope.stage })
  const document = documentFor('runtime-boundary-policy.json', scope)
  log(`==> ${arn}`)

  if (!(await aws.present(['iam', 'get-policy', '--policy-arn', arn]))) {
    await aws.apply('Could not create the runtime boundary', [
      'iam',
      'create-policy',
      '--policy-name',
      `boxlite-${scope.stage}-runtime-boundary`,
      '--description',
      'Maximum data-plane permissions for roles created by the BoxLite SST stack',
      '--policy-document',
      document,
    ])
    log('    created')
    return arn
  }

  const { Versions } = await aws.read(`Could not list ${arn}'s versions`, ['iam', 'list-policy-versions', '--policy-arn', arn])
  const disposable = Versions.filter((version: any) => !version.IsDefaultVersion).sort(
    (a: any, b: any) => new Date(a.CreateDate).getTime() - new Date(b.CreateDate).getTime(),
  )
  // About to add one more version; prune the oldest non-default ones until there is room for it.
  const overflow = Math.max(0, Versions.length - MANAGED_POLICY_VERSION_LIMIT + 1)
  for (const version of disposable.slice(0, overflow)) {
    await aws.apply(`Could not prune ${arn}'s version ${version.VersionId}`, [
      'iam',
      'delete-policy-version',
      '--policy-arn',
      arn,
      '--version-id',
      version.VersionId,
    ])
  }

  await aws.apply('Could not update the runtime boundary', [
    'iam',
    'create-policy-version',
    '--policy-arn',
    arn,
    '--policy-document',
    document,
    '--set-as-default',
  ])
  log('    updated')
  return arn
}

/**
 * The role GitHub Actions deploys as.
 *
 * Trust is per stage: every job that assumes this declares `environment:
 * <stage>` (mdeploy.yml), so GitHub's `sub` claim carries `environment:<stage>`
 * rather than the branch form — a role trusting only the branch form rejects
 * them all with `Not authorized to perform sts:AssumeRoleWithWebIdentity`,
 * which reads as a missing role rather than as the claim mismatch it is.
 *
 * Unlike backoffice/commerce's own deploy role, `<REPO>` renders from a value
 * resolved per invocation (bootstrap.ts's resolveRepo) rather than a name
 * fixed in a committed github.json: a community fork must be able to
 * bootstrap its own copy of this role, trusting its own repository, without
 * editing a file first.
 */
const ensureDeployRole = async ({ aws, scope, log }: { aws: Aws; scope: DocumentScope; log: (line: string) => void }): Promise<string> => {
  const name = githubDeployRoleName(scope.stage)
  const trust = documentFor('deploy-role-trust.json', scope)
  log(`==> ${name}`)

  if (await aws.present(['iam', 'get-role', '--role-name', name])) {
    await aws.apply(`Could not update ${name}'s trust policy`, ['iam', 'update-assume-role-policy', '--role-name', name, '--policy-document', trust])
    await aws.apply(`Could not set ${name}'s session duration`, [
      'iam',
      'update-role',
      '--role-name',
      name,
      '--max-session-duration',
      DEPLOY_SESSION_DURATION_SECONDS,
    ])
    log('    trust policy updated')
  } else {
    await aws.apply(`Could not create ${name}`, [
      'iam',
      'create-role',
      '--role-name',
      name,
      '--assume-role-policy-document',
      trust,
      '--max-session-duration',
      DEPLOY_SESSION_DURATION_SECONDS,
      '--description',
      'Short-lived role used only by the guarded BoxLite GitHub deployment environment',
    ])
    log('    created')
  }

  // The fence that makes CreateBoundedBoxLiteRoles/SetBoxLiteRoleBoundary satisfiable: without
  // it, SST's own task and execution roles are created with no boundary, no Allow matches, and
  // the deploy dies on iam:CreateRole (deployment/role-boundary.ts, deployment/verify-role.ts).
  await aws.apply(`Could not write ${name}'s inline policy`, [
    'iam',
    'put-role-policy',
    '--role-name',
    name,
    '--policy-name',
    'boxlite-sst-deploy',
    '--policy-document',
    documentFor('deploy-role-policy.json', scope),
  ])
  log('    inline policy written')

  return `arn:aws:iam::${scope.accountId}:role/${name}`
}

/**
 * The Api's image repository, named through the same helper artifacts/api.ts
 * reads it back with (apiImageRepository) — one spelling, not two kept in
 * agreement by a test the way a CloudFormation resource name and a JS
 * constant needed to be.
 */
const ensureApiImageRepository = async ({ aws, stage, log }: { aws: Aws; stage: string; log: (line: string) => void }): Promise<string> => {
  const repository = apiImageRepository({ app: 'boxlite', stage })
  log(`==> ${repository}`)

  if (await aws.present(['ecr', 'describe-repositories', '--repository-names', repository])) {
    log('    already exists')
    return repository
  }
  // Immutable tags are what make a published commit mean exact bytes; scan on push is what
  // gives mbuild's severity gate something to read. Never deleted or replaced by this script —
  // a stronger guarantee than CloudFormation's DeletionPolicy: Retain gave the same resource.
  await aws.apply(`Could not create ${repository}`, [
    'ecr',
    'create-repository',
    '--repository-name',
    repository,
    '--image-tag-mutability',
    'IMMUTABLE',
    '--image-scanning-configuration',
    'scanOnPush=true',
    '--encryption-configuration',
    'encryptionType=AES256',
  ])
  log('    created')
  return repository
}

/**
 * The Runner's build-mode install artifact bucket, named through the same
 * helper artifacts/runner.ts reads it back with (runnerArtifactsBucketName),
 * for the same one-spelling reason the Api repository above is.
 *
 * `create-bucket`'s location constraint is the one AWS CLI quirk this file
 * cannot fold into a plain `present`/`apply`: passing
 * `--create-bucket-configuration LocationConstraint=us-east-1` in that one
 * region is itself an error, because it is the CLI's implicit default.
 * CloudFormation's `AWS::S3::Bucket` absorbed that distinction silently; a
 * reconcile script has to name it.
 */
const ensureArtifactsBucket = async ({
  aws,
  stage,
  accountId,
  region,
  log,
}: {
  aws: Aws
  stage: string
  accountId: string
  region: string
  log: (line: string) => void
}): Promise<string> => {
  const bucket = runnerArtifactsBucketName({ app: 'boxlite', stage, accountId })
  log(`==> ${bucket}`)

  if (await aws.present(['s3api', 'head-bucket', '--bucket', bucket])) {
    log('    already exists')
  } else {
    const locationConfiguration = region === 'us-east-1' ? [] : ['--create-bucket-configuration', `LocationConstraint=${region}`]
    await aws.apply(`Could not create ${bucket}`, ['s3api', 'create-bucket', '--bucket', bucket, ...locationConfiguration])
    log('    created')
  }

  await aws.apply(`Could not enable encryption on ${bucket}`, [
    's3api',
    'put-bucket-encryption',
    '--bucket',
    bucket,
    '--server-side-encryption-configuration',
    JSON.stringify({ Rules: [{ ApplyServerSideEncryptionByDefault: { SSEAlgorithm: 'AES256' } }] }),
  ])
  await aws.apply(`Could not enable versioning on ${bucket}`, [
    's3api',
    'put-bucket-versioning',
    '--bucket',
    bucket,
    '--versioning-configuration',
    'Status=Enabled',
  ])
  await aws.apply(`Could not block public access on ${bucket}`, [
    's3api',
    'put-public-access-block',
    '--bucket',
    bucket,
    '--public-access-block-configuration',
    'BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true',
  ])
  // Superseded versions only, never current ones: buildRunnerUserData re-fetches the
  // commit-keyed tarball on every instance launch, so expiring a *current* object would make a
  // later replacement fail to boot — including for the stage that is currently deployed.
  await aws.apply(`Could not set the lifecycle rule on ${bucket}`, [
    's3api',
    'put-bucket-lifecycle-configuration',
    '--bucket',
    bucket,
    '--lifecycle-configuration',
    JSON.stringify({
      Rules: [{ ID: 'expire-superseded-runner-builds', Filter: { Prefix: 'runner/' }, Status: 'Enabled', NoncurrentVersionExpiration: { NoncurrentDays: 30 } }],
    }),
  ])
  log('    encryption, versioning, public-access block and lifecycle rule set')
  return bucket
}

export type AwsBootstrapInput = {
  run: Run
  repo: string
  stage: string
  accountId: string
  region: string
  log: (line: string) => void
}

/** What was created, mirroring GcpBootstrapResult's shape in gcp.ts. */
export type AwsBootstrapResult = {
  roleArn: string
  boundaryArn: string
  repositoryName: string
  bucketName: string
}

/**
 * One invocation: everything an `mdeploy`/`mbuild` run on this stage needs on
 * AWS and cannot create for itself. Mirrors bootstrapGcp's shape in gcp.ts —
 * one exported entry point, reconciling rather than creating, called once
 * from bootstrap.ts's main().
 */
export const bootstrapAws = async ({ run, repo, stage, accountId, region, log }: AwsBootstrapInput): Promise<AwsBootstrapResult> => {
  const aws = awsFor({ run, region })
  const scope: DocumentScope = { repo, stage, accountId, region }

  const boundaryArn = await ensureRuntimeBoundary({ aws, scope, log })
  const roleArn = await ensureDeployRole({ aws, scope, log })
  const repositoryName = await ensureApiImageRepository({ aws, stage, log })
  const bucketName = await ensureArtifactsBucket({ aws, stage, accountId, region, log })

  return { roleArn, boundaryArn, repositoryName, bucketName }
}
