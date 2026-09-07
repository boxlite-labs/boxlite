# bootstrap/aws — the out-of-band AWS bootstrap

What has to exist on AWS before a stage's first `sst deploy`, because the deploy
needs it in order to run: the role that deploys cannot create itself, the
runtime permissions boundary every role SST creates must carry has to exist
before the first apply, the Api image repository has to exist before CI can
push into it, and the Runner artifact bucket has to exist before a build-mode
install can read from it.

The JSON files here are the exact IAM documents, in the same shape
`boxlite-backoffice` and `boxlite-commerce` use for their own AWS bootstrap
(`apps/infra/iam/*.json` there): checked-in, applied by `../aws.ts` through the
AWS CLI, never by `aws cloudformation deploy`. This replaced a single
CloudFormation stack — see git history for `bootstrap/aws/github-deploy-role.yaml`
if you need the prior shape. Unlike backoffice/commerce, nothing here is
account-wide: the runtime boundary is per stage
(`boxlite-<stage>-runtime-boundary`, `deployment/environment.ts`'s
`runtimeBoundaryPolicyArn`), because this stack — unlike backoffice's or
commerce's, which were adopted rather than created — provisions its own roles
from nothing and needs a boundary before the first one exists.

`npm run bootstrap -- --stage=dev` renders `<REPO>`, `<STAGE>`, `<ACCOUNT>` and
`<REGION>` into these documents and reconciles AWS to match. Re-running is how
an edit here reaches AWS — that is the intended way to change a grant: edit the
file, re-run bootstrap, commit both. `<REPO>` is resolved per invocation
(`resolveRepo` in `bootstrap.ts`, defaulting to `gh repo view`) rather than
declared in a committed `github.json` the way backoffice/commerce pin it: a
community fork must be able to bootstrap its own copy of this role trusting its
own repository without editing a file first.

## `deploy-role-trust.json` — who may assume `boxlite-<stage>-github-deploy`

Trust is pinned through GitHub's OIDC provider to one repository and one stage
Environment (`repo:<owner>/<repo>:environment:<stage>`). Every job in
`mdeploy.yml` that assumes this role declares `environment: <stage>`, and
GitHub issues such a job exactly that `sub` claim — a role trusting only the
branch form rejects them all with `Not authorized to perform
sts:AssumeRoleWithWebIdentity`, which reads as a missing role rather than as
the claim mismatch it is. It also means dev's deploy role can never be assumed
from a run against prod. No long-lived credential exists anywhere.

## `runtime-boundary-policy.json` — `boxlite-<stage>-runtime-boundary`

The managed policy every role SST creates must carry (the `$transform` in
`stack/deploy.ts`). Maximum data-plane permissions for a *running* workload —
control-plane channels every task needs (ECR pull, CloudWatch Logs, SSM/EC2
control messages for the Runner), this stage's own secrets and KMS aliases, the
buckets this stage and the Runner volumes own, and the ability to assume this
stage's own runtime roles. No IAM mutation of any kind — that is the deploy
role's job, never a running workload's.

A customer-managed policy update is a new policy *version*, not an overwrite,
and AWS caps a managed policy at 5 versions — `aws.ts`'s `ensureRuntimeBoundary`
prunes the oldest non-default version before creating a new one so a stage
bootstrapped more than 5 times does not fail with `LimitExceeded`. This is not
a concern backoffice/commerce's own bootstrap has: both assume one boundary
already exists account-wide and never touch its lifecycle.

## `deploy-role-policy.json` — the deploy role's own inline policy

PowerUser-shaped access to this SST stack's own resource types
(`BoxLiteAwsControlPlane`), plus IAM write that comes back only inside a fence.
Two escalation paths are closed with `Deny`, because an `Allow` cannot express
"every `boxlite-<stage>-*` resource except this one":

- **`CreateBoundedBoxLiteRoles`/`SetBoxLiteRoleBoundary`** only allow creating
  or reboundarying a role when the request attaches this stage's own runtime
  boundary — the fence that makes `stack/deploy.ts`'s `$transform` satisfiable.
  Without it every SST-managed role fails identically on
  `iam:CreateRole`/`AccessDenied` (see `deployment/role-boundary.ts`,
  `deployment/verify-role.ts`, the CI preflight this feeds).
- **`DenySelfPrivilegeEscalation`** denies IAM writes against this role and the
  boundary policy themselves, across *every* stage (`boxlite-*-github-deploy`,
  `boxlite-*-runtime-boundary`) — deliberately not scoped to just this stage,
  as a backstop against a dev-bound job ever rewriting prod's trust policy or
  widening the boundary a bounded role is created under.

Everything s3/ssm/secretsmanager is deliberately **stage-scoped** rather than
riding on a broader `s3:*`/`ssm:*`/`secretsmanager:*` grant the way
backoffice/commerce's `PowerUserAccess`-based policy does: SST's backing store
holds every stage's state and secrets under one shared bucket and one parameter
tree, so an account-wide grant would let a job bound to `dev`'s Environment
read `prod`'s. `SstStateForThisStage`/`SstStateObjectsForThisStage` and
`SecretsForThisStage` narrow that back down to this stage's own keys and
secrets before this role ever reaches PowerUser-shaped breadth on anything
else.

Two grants are wider than they look, both stated here rather than left to read
as scoped:

- **`cloudfront-keyvaluestore:*`** is its own statement because an IAM wildcard
  never crosses the `service:` colon — `cloudfront:*` does not reach it.
  `sst.aws.Router` keeps its route table in a CloudFront KeyValueStore, so
  every apply calls `DescribeKeyValueStore`; without this grant every apply
  fails there while every preview passes, since a preview makes no KV call.
- **`RunnerCommandChannel`**'s `ssm:SendCommand` targets `instance/*`
  account-wide: an instance ARN carries no stage, so there is no way to scope
  it here without a condition on an instance tag the Runner launch template
  would need to set. A job bound to one stage can run a shell command on any
  instance in the account — documented in `docs/security.md`, not left to read
  as stage-scoped.

## Api image repository and Runner artifacts bucket

Not IAM documents — a repository and a bucket, created imperatively by
`aws.ts`'s `ensureApiImageRepository`/`ensureArtifactsBucket` through
`artifacts/api.ts`'s `apiImageRepository` and `artifacts/runner.ts`'s
`runnerArtifactsBucketName`, the same naming helpers a deploy uses to find them
— one spelling, not two kept in agreement by a test. Immutable tags and scan-on
push on the repository are what make a published commit mean exact bytes and
give `mbuild`'s severity gate something to read. The bucket's lifecycle rule
expires only *superseded* object versions: `buildRunnerUserData` re-fetches the
commit-keyed tarball on every instance launch, so expiring a *current* object
would make a later replacement fail to boot, including for the stage currently
deployed. Neither resource is ever deleted or replaced by this script — a
stronger guarantee than CloudFormation's `DeletionPolicy: Retain` gave the same
resources, since nothing here ever calls `delete-repository` or removes a
bucket at all.

## Changing these

Edit the JSON, then `npm run bootstrap -- --stage=<stage>` to reach AWS.
Running it needs IAM write in the account; editing the files does not, so
authoring and applying a change can be, and often are, two different people.
