// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

/*
 * `npm run bootstrap -- --stage <stage>` when `mstage.config.json` says this
 * stage's home is `gcp`.
 *
 * Every item here exists for the same reason its AWS counterpart in
 * `bootstrap.ts` does: the deploy cannot create it, because the deploy needs
 * it in order to run.
 *
 *   the enabled APIs         nothing can be created through an API that is off
 *   the state bucket         Pulumi keeps its state there and mstage its store,
 *                            so it exists before either has anywhere to write
 *   mstage-bootstrap         the record that names that bucket; the GCS
 *                            counterpart of AWS's `/sst/bootstrap`
 *   mstage-passphrase-*      what the store is sealed with. Generated once and
 *                            never rotated here: rotating it would make every
 *                            stored value unreadable
 *   the identity pool        CI proves a GitHub OIDC token and nothing else, so
 *                            the pool that trusts it cannot come from a deploy
 *                            that has not been authorised yet
 *   the deployer             the identity mdeploy runs as
 *   the image publisher      assumed by mbuild.yml, which runs before any
 *                            stage is deployed at all
 *   the docker repository    a first publish would fail on push into a
 *                            repository nothing created
 *
 * Reconciles rather than creates, exactly like the AWS side: re-running is how
 * an edit to the role list below reaches GCP.
 *
 * Talks to `gcloud` rather than adding the Google SDKs to this directory's
 * dependency surface — the same choice `bootstrap.ts` makes about `aws`, and
 * for the same reason: the CLI is a prerequisite for operating the project at
 * all, and this runs on a workstation.
 *
 * The state bucket's name is never printed. It is protected by IAM either way,
 * but it is also the one name that says where every stage's secrets live, and
 * this repository keeps it out of terminal output and CI logs on both clouds.
 *
 * Modelled on boxlite-backoffice's `apps/infra/iam/src/gcp.ts` (as of
 * 227d9f5), the prior art `mstage`'s own GCP fixes were ported from in
 * 50665ad0. The role and service lists below are BoxLite's own, though,
 * derived from what `mdeploy/stack/providers/gcp/*.ts` actually instantiates
 * rather than copied — this stack has a cache and no pub/sub, backoffice's has
 * the reverse.
 */

import { randomBytes } from 'node:crypto'

export class GcpBootstrapError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'GcpBootstrapError'
  }
}

/** One external command. A non-zero exit is reported, never thrown away. */
export type RunResult = { code: number; stdout: string; stderr: string }

/**
 * `stdin` is how a secret reaches a command without passing through argv,
 * which is readable in the process table for as long as the command runs.
 * Here it carries the passphrase the store is sealed with and the JSON record
 * naming the state bucket.
 */
export type RunOptions = { stdin?: string }
export type Run = (command: string, args: string[], options?: RunOptions) => Promise<RunResult>

/** Where every workload identity pool in this file lives. Pools are not regional. */
const POOL_LOCATION = 'global'

/** One pool per project, holding the one provider that trusts GitHub. Named after `app`. */
const POOL_PROVIDER = 'github'

/** The record mstage reads to find the store, and the key inside it. */
const BOOTSTRAP_SECRET = 'mstage-bootstrap'

/**
 * Every API a resource in `mdeploy/stack/providers/gcp/` needs.
 *
 * Derived from that bundle rather than from a checklist: `grep -hoE "new
 * gcp\.[A-Za-z0-9]+\.[A-Za-z0-9]+" mdeploy/stack/providers/gcp/*.ts` names
 * every resource type the stack builds, and each line below names what asked
 * for it. An API left off does not fail at plan time: the apply reaches that
 * resource and returns `SERVICE_DISABLED`, halfway through.
 */
const SERVICES = [
  // Network, Subnetwork, Router, RouterNat, Firewall, Address, GlobalAddress,
  // BackendService, RegionBackendService, the two ForwardingRules,
  // TargetHttpsProxy, URLMap, ManagedSslCertificate, the runner InstanceTemplate
  // and RegionInstanceGroupManager, and their Disks.
  'compute.googleapis.com',
  // cloudrunv2.Service for the api and the otel-collector.
  'run.googleapis.com',
  // redis.Instance: the cache.
  'redis.googleapis.com',
  // sql.DatabaseInstance, sql.Database, sql.User.
  'sqladmin.googleapis.com',
  // servicenetworking.Connection: the private services access the database
  // instance is reachable through.
  'servicenetworking.googleapis.com',
  // secretmanager.Secret and SecretVersion — the stack's own, and the two this
  // file creates.
  'secretmanager.googleapis.com',
  // The state bucket, for both Pulumi's state and mstage's store, and the
  // stack's own storage.Bucket.
  'storage.googleapis.com',
  // The repository a stage's images are published into.
  'artifactregistry.googleapis.com',
  // serviceaccount.Account and the workload identity pool this file creates.
  'iam.googleapis.com',
  // Minting tokens for a federated identity, which is how CI signs in at all.
  'iamcredentials.googleapis.com',
  'sts.googleapis.com',
  // logging.Metric, and monitoring.AlertPolicy built on top of them.
  'logging.googleapis.com',
  'monitoring.googleapis.com',
  // projects.IAMMember: the stack grants project-level roles to its own
  // service accounts.
  'cloudresourcemanager.googleapis.com',
]

/**
 * What the deploy identity may do, one role per group of resources that needs it.
 *
 * Wide on purpose, for now. The AWS side reaches the same place with
 * PowerUserAccess plus a fenced inline policy (`bootstrap/aws/github-deploy-role.yaml`);
 * GCP has no such umbrella, so this is the list of admin roles the bundle's
 * resource types actually require. Narrowing this is the step to take once a
 * first apply has succeeded and the audit log says which permissions were
 * used — guessing a smaller set beforehand produces a deploy that fails in ten
 * places in turn.
 *
 * SELF-ESCALATION, stated rather than buried. `resourcemanager.projectIamAdmin`
 * together with `iam.serviceAccountAdmin` and `iam.serviceAccountUser` lets
 * this identity grant itself any role in the project, including owner. The AWS
 * half denies exactly that path — the inline policy allows `iam:CreateRole`
 * only when the request attaches the account's permissions boundary, and
 * denies every user and group write — and GCP has no boundary to attach, so
 * nothing here reproduces the fence.
 *
 * It is not gratuitous: the stack grants project-level roles itself
 * (`gcp.projects.IAMMember` in `stack/providers/gcp/api.ts`, `database.ts`,
 * `cache.ts` and `clickhouse.ts`), and the members are service accounts the
 * stack creates, so bootstrap cannot pre-create those bindings and hand the
 * deployer something narrower.
 *
 * What stands in for the fence today is who may assume this identity at all:
 * only a job in this repository, pinned by immutable owner and repository id
 * (`attributeCondition` below), declaring this stage's environment. The real
 * fix is an organization policy restricting which roles may be granted in the
 * project, which is org-level and outside this repository.
 */
const DEPLOYER_ROLES = [
  // Every compute.* resource: the network, the NAT, the firewall rules, the
  // load balancer in front of the proxy, and the runners.
  'roles/compute.admin',
  // The api and the otel-collector.
  'roles/run.admin',
  // The cache.
  'roles/redis.admin',
  // The database instance, its database and its user.
  'roles/cloudsql.admin',
  // The private services access connection the database sits behind.
  'roles/servicenetworking.networksAdmin',
  // The stack's own secrets, and reading the two this file created.
  'roles/secretmanager.admin',
  // Pulumi's state and mstage's store, both in the bucket below, and the
  // stack's own buckets.
  'roles/storage.admin',
  // Creating the service accounts each workload runs as...
  'roles/iam.serviceAccountAdmin',
  // ...and being allowed to run a revision or an instance as one of them,
  // which is a separate permission from having created it.
  'roles/iam.serviceAccountUser',
  // gcp.projects.IAMMember: the stack grants project-level roles to those
  // service accounts.
  'roles/resourcemanager.projectIamAdmin',
  // The log-based metrics the alert policies are built on, and the policies.
  'roles/logging.configWriter',
  'roles/monitoring.editor',
]

/** The publisher pushes images and reads nothing else. */
const PUBLISHER_ROLES = ['roles/artifactregistry.writer']

/**
 * Which GitHub repository may federate in.
 *
 * Resolved at bootstrap time from `gh api repos/<owner>/<repo>` rather than
 * declared in a checked-in file: `bootstrap.ts`'s AWS half already resolves
 * `owner/repo` per invocation (`resolveRepo`, defaulting to `gh repo view`) so
 * a community fork bootstraps itself rather than someone else's repository,
 * and the GCP half must not silently narrow that to one hardcoded fork.
 */
export type GitHubRepository = {
  issuer: string
  owner: string
  ownerId: string
  repository: string
  repositoryId: string
}

/**
 * The fence that makes federation safe at all.
 *
 * Without a condition any GitHub Actions job anywhere could present a token
 * this provider accepts. Pinned to the numeric ids rather than the names: an
 * owner or repository name can be transferred or re-created, an id cannot.
 */
export const attributeCondition = (github: GitHubRepository): string =>
  `assertion.repository_owner_id == '${github.ownerId}' && assertion.repository_id == '${github.repositoryId}'`

/**
 * Which claims become attributes a binding can name.
 *
 * `environment` and `ref` are the two claim shapes GitHub actually issues: a
 * job that declares an environment presents the first, and one that does
 * not — publish workflows running on push to main — presents the second.
 */
const ATTRIBUTE_MAPPING = [
  'google.subject=assertion.sub',
  'attribute.environment=assertion.environment',
  'attribute.ref=assertion.ref',
].join(',')

/** The gcloud calls this makes, split by what a failure means. */
type Gcloud = {
  /** Whether a resource is already there. Absence is an answer, not a failure. */
  present: (args: string[]) => Promise<boolean>
  /** A question with an answer worth reading. An empty string means "nothing". */
  read: (args: string[]) => Promise<string>
  /**
   * A question whose absence is an answer and whose failure is not.
   *
   * `read` cannot tell the two apart, and for the bootstrap record they must
   * be: a transient failure read as "no record" makes this file generate a
   * second bucket name and strand every stored value in the first one.
   */
  readOptional: (what: string, args: string[]) => Promise<string | null>
  /** A change. A non-zero exit fails the run, carrying the CLI's own message. */
  apply: (what: string, args: string[], stdin?: string) => Promise<void>
}

const gcloudFor = ({ run, project }: { run: Run; project: string }): Gcloud => {
  // `--quiet` so nothing waits for a prompt in CI, and the project on every
  // call rather than relying on whatever `gcloud config` holds locally.
  const call = (args: string[], stdin?: string) =>
    run('gcloud', [...args, '--project', project, '--quiet'], stdin === undefined ? undefined : { stdin })
  return {
    async present(args) {
      return (await call(args)).code === 0
    },
    async read(args) {
      const result = await call(args)
      if (result.code !== 0) return ''
      return result.stdout.trim()
    },
    async readOptional(what, args) {
      const result = await call(args)
      if (result.code === 0) return result.stdout.trim()
      // gcloud says NOT_FOUND for a resource that is simply not there, and
      // something else for a permission problem or an unreachable API.
      if (/NOT_FOUND|was not found|does not exist/i.test(result.stderr)) return null
      throw new GcpBootstrapError(`${what}: ${result.stderr.trim() || `gcloud exited ${result.code}`}`)
    },
    async apply(what, args, stdin) {
      const result = await call(args, stdin)
      if (result.code === 0) return
      throw new GcpBootstrapError(`${what}: ${result.stderr.trim() || `gcloud exited ${result.code}`}`)
    },
  }
}

/** Turns on what is off, and says nothing about what was already on. */
const ensureServices = async ({ gcloud, log }: { gcloud: Gcloud; log: (line: string) => void }): Promise<void> => {
  log('==> services')
  const enabled = new Set(
    (await gcloud.read(['services', 'list', '--enabled', '--format=value(config.name)'])).split('\n').filter(Boolean),
  )
  const missing = SERVICES.filter((service) => !enabled.has(service))
  if (missing.length === 0) {
    log(`    all ${SERVICES.length} already enabled`)
    return
  }
  // One call: enabling a service takes seconds and gcloud accepts the whole set.
  await gcloud.apply(`Could not enable ${missing.join(', ')}`, ['services', 'enable', ...missing])
  log(`    enabled ${missing.join(', ')}`)
}

/**
 * The bucket that holds Pulumi's state and mstage's store, and the record that
 * names it.
 *
 * Discovered before it is created: if the record already names a bucket, that
 * one is reconciled and kept. Generating a second name on a re-run would leave
 * every stored value behind in the first bucket while the record pointed at an
 * empty one — the failure would look like a stage that had never been
 * configured.
 */
const ensureStateBucket = async ({
  gcloud,
  region,
  log,
}: {
  gcloud: Gcloud
  region: string
  log: (line: string) => void
}): Promise<void> => {
  log(`==> ${BOOTSTRAP_SECRET}`)
  const recorded = await gcloud.readOptional(`Could not read ${BOOTSTRAP_SECRET}`, [
    'secrets',
    'versions',
    'access',
    'latest',
    '--secret',
    BOOTSTRAP_SECRET,
  ])
  let bucket: string | null = null
  if (recorded) {
    let parsed: { state?: string }
    try {
      parsed = JSON.parse(recorded)
    } catch {
      throw new GcpBootstrapError(`${BOOTSTRAP_SECRET} is not valid JSON. mstage reads {"state":"<bucket>"} from it.`)
    }
    if (!parsed.state) throw new GcpBootstrapError(`${BOOTSTRAP_SECRET} names no state bucket.`)
    bucket = parsed.state
    log('    already names a bucket')
  }

  if (!bucket) {
    // Not guessable, and not derived from the project: the name is the one
    // string that says where every stage's secrets live.
    bucket = `mstage-state-${randomBytes(8).toString('hex')}`
    log('    generating a new one')
  }

  if (await gcloud.present(['storage', 'buckets', 'describe', `gs://${bucket}`])) {
    log('    bucket already exists')
  } else {
    await gcloud.apply('Could not create the state bucket', [
      'storage',
      'buckets',
      'create',
      `gs://${bucket}`,
      `--location=${region}`,
      // No object ACLs: access is the bucket's IAM and nothing else, so a
      // mis-set ACL cannot open one object.
      '--uniform-bucket-level-access',
      '--public-access-prevention',
    ])
    log('    bucket created')
  }
  // Versioning is what makes a pinned read possible, and what makes a Pulumi
  // state overwrite recoverable.
  await gcloud.apply('Could not turn on versioning for the state bucket', [
    'storage',
    'buckets',
    'update',
    `gs://${bucket}`,
    '--versioning',
  ])
  log('    versioning on')

  if (!recorded) {
    await gcloud.apply(
      `Could not create ${BOOTSTRAP_SECRET}`,
      ['secrets', 'create', BOOTSTRAP_SECRET, '--replication-policy=automatic', '--data-file=-'],
      JSON.stringify({ state: bucket }),
    )
    log('    record written')
  }
}

/**
 * What the store is sealed with, created once and never replaced.
 *
 * A second version would not rotate anything — it would make every value
 * already in the store undecryptable, because the store is sealed with the
 * version that was current when it was written. So an existing secret is left
 * exactly alone.
 */
const ensurePassphrase = async ({
  gcloud,
  app,
  stage,
  log,
}: {
  gcloud: Gcloud
  app: string
  stage: string
  log: (line: string) => void
}): Promise<void> => {
  const name = `mstage-passphrase-${app}-${stage}`
  log(`==> ${name}`)
  if (await gcloud.present(['secrets', 'describe', name])) {
    log('    already exists, left alone')
    return
  }
  await gcloud.apply(
    `Could not create ${name}`,
    ['secrets', 'create', name, '--replication-policy=automatic', '--data-file=-'],
    // Through stdin, never argv: an argument is visible in the process table.
    randomBytes(32).toString('base64'),
  )
  log('    created')
}

/** The pool and the one provider in it that trusts GitHub's OIDC tokens. */
const ensurePool = async ({
  gcloud,
  pool: POOL,
  github,
  log,
}: {
  gcloud: Gcloud
  pool: string
  github: GitHubRepository
  log: (line: string) => void
}): Promise<void> => {
  const pool = ['iam', 'workload-identity-pools']
  log(`==> ${POOL}/${POOL_PROVIDER}`)

  if (!(await gcloud.present([...pool, 'describe', POOL, `--location=${POOL_LOCATION}`]))) {
    await gcloud.apply(`Could not create the ${POOL} pool`, [
      ...pool,
      'create',
      POOL,
      `--location=${POOL_LOCATION}`,
      '--display-name=BoxLite CI',
    ])
    log('    pool created')
  }

  const condition = attributeCondition(github)
  const provider = [...pool, 'providers']
  const settings = [
    `--location=${POOL_LOCATION}`,
    `--workload-identity-pool=${POOL}`,
    `--issuer-uri=${github.issuer}`,
    `--attribute-mapping=${ATTRIBUTE_MAPPING}`,
    `--attribute-condition=${condition}`,
    /*
     * No `--allowed-audiences`. An explicit list replaces GCP's default rather
     * than adding to it, and the default is the provider's own resource URL —
     * exactly what `google-github-actions/auth` requests when no `audience:`
     * input is given, as none is in `.github/workflows/mdeploy.yml` or
     * `mbuild.yml`. Pinning the `projects/-` spelling instead would reject
     * every token the action actually mints.
     */
  ]
  if (
    await gcloud.present([
      ...provider,
      'describe',
      POOL_PROVIDER,
      `--location=${POOL_LOCATION}`,
      `--workload-identity-pool=${POOL}`,
    ])
  ) {
    // Updated rather than skipped: the condition is the fence, and a rerun
    // against a repository whose owner/repo id changed has to reach GCP.
    await gcloud.apply(`Could not update the ${POOL_PROVIDER} provider`, [
      ...provider,
      'update-oidc',
      POOL_PROVIDER,
      ...settings,
    ])
    log('    provider updated')
  } else {
    await gcloud.apply(`Could not create the ${POOL_PROVIDER} provider`, [
      ...provider,
      'create-oidc',
      POOL_PROVIDER,
      ...settings,
    ])
    log('    provider created')
  }
}

const serviceAccountEmail = (id: string, project: string): string => `${id}@${project}.iam.gserviceaccount.com`

/**
 * A service account, reconciled by existence alone: nothing here would change one.
 *
 * `describe` takes the full email and `create` takes the bare id — the two
 * commands disagree, and gcloud rejects the other spelling rather than
 * answering "absent". Probing with the id would make every re-run try to
 * create an account that was already there, which is the opposite of the
 * reconcile this file promises.
 */
const ensureServiceAccount = async ({
  gcloud,
  id,
  email,
  description,
  log,
}: {
  gcloud: Gcloud
  id: string
  email: string
  description: string
  log: (line: string) => void
}): Promise<void> => {
  if (await gcloud.present(['iam', 'service-accounts', 'describe', email])) {
    log('    already exists')
    return
  }
  await gcloud.apply(`Could not create the ${id} service account`, [
    'iam',
    'service-accounts',
    'create',
    id,
    `--description=${description}`,
  ])
  log('    created')
}

/** Project-level roles. `add-iam-policy-binding` is idempotent by design. */
const grantProjectRoles = async ({
  gcloud,
  project,
  email,
  roles,
  log,
}: {
  gcloud: Gcloud
  project: string
  email: string
  roles: string[]
  log: (line: string) => void
}): Promise<void> => {
  for (const role of roles) {
    await gcloud.apply(`Could not grant ${role} to ${email}`, [
      'projects',
      'add-iam-policy-binding',
      project,
      `--member=serviceAccount:${email}`,
      `--role=${role}`,
      // Without this gcloud prompts for a condition, which never returns in CI.
      '--condition=None',
    ])
  }
  log(`    ${roles.length} project roles granted`)
}

/**
 * Lets one federated principal act as one service account.
 *
 * `principalSet` rather than `principal`: the member is every token whose
 * mapped attribute has this value, which is what makes "any job in this
 * repository declaring environment dev" expressible at all.
 */
const allowImpersonation = async ({
  gcloud,
  email,
  projectNumber,
  pool: POOL,
  attribute,
  value,
}: {
  gcloud: Gcloud
  email: string
  projectNumber: string
  pool: string
  attribute: string
  value: string
}): Promise<void> => {
  const member =
    `principalSet://iam.googleapis.com/projects/${projectNumber}/locations/${POOL_LOCATION}` +
    `/workloadIdentityPools/${POOL}/attribute.${attribute}/${value}`
  await gcloud.apply(`Could not let ${attribute}/${value} act as ${email}`, [
    'iam',
    'service-accounts',
    'add-iam-policy-binding',
    email,
    `--member=${member}`,
    '--role=roles/iam.workloadIdentityUser',
  ])
}

/** The repository a stage's images are published into. */
const ensureRepository = async ({
  gcloud,
  repository,
  region,
  log,
}: {
  gcloud: Gcloud
  repository: string
  region: string
  log: (line: string) => void
}): Promise<void> => {
  log(`==> ${repository}`)
  const args = ['artifacts', 'repositories']
  if (await gcloud.present([...args, 'describe', repository, `--location=${region}`])) {
    log('    already exists')
    return
  }
  await gcloud.apply(`Could not create the ${repository} repository`, [
    ...args,
    'create',
    repository,
    '--repository-format=docker',
    `--location=${region}`,
    '--description=BoxLite’s api and otel-collector images',
  ])
  log('    created')
}

export type GcpBootstrapInput = {
  run: Run
  /** The project this stage lives in, from `mstage.config.json`. */
  project: string
  /** Where the stage lives, which is also where its bucket and repository go. */
  region: string
  app: string
  stage: string
  /** Which docker repository this stage publishes into, from `mbuild.config.json`. */
  repository: string
  github: GitHubRepository
  log: (line: string) => void
}

/** What was created, for the caller to wire into GitHub. gcp.ts never calls `gh` itself. */
export type GcpBootstrapResult = {
  workloadIdentityProvider: string
  deployerEmail: string
  publisherEmail: string
}

/**
 * One invocation: everything a `mdeploy`/`mbuild` run on this stage needs and
 * cannot create for itself.
 *
 * Returns what it made rather than printing `gh` commands: `bootstrap.ts`
 * already writes the AWS deploy role's ARN straight into the GitHub
 * Environment (`ghEnvironmentVariableSet`) instead of asking the operator to
 * paste it, and the GCP half follows the same, already-established shape
 * instead of introducing a second UX for the same command.
 */
export const bootstrapGcp = async ({
  run,
  project,
  region,
  app,
  stage,
  repository,
  github,
  log,
}: GcpBootstrapInput): Promise<GcpBootstrapResult> => {
  const gcloud = gcloudFor({ run, project })

  // Needed for every principalSet below, and a project that cannot be described
  // is a project these credentials cannot act on — worth failing on first.
  const projectNumber = await gcloud.read(['projects', 'describe', project, '--format=value(projectNumber)'])
  if (!projectNumber) {
    throw new GcpBootstrapError(
      `Could not read the number of project ${project}. Check that it exists and that these credentials can see it.`,
    )
  }

  const pool = app

  await ensureServices({ gcloud, log })
  await ensureStateBucket({ gcloud, region, log })
  await ensurePassphrase({ gcloud, app, stage, log })
  await ensurePool({ gcloud, pool, github, log })

  const deployer = `boxlite-${stage}-deploy`
  const deployerEmail = serviceAccountEmail(deployer, project)
  log(`==> ${deployer}`)
  await ensureServiceAccount({
    gcloud,
    id: deployer,
    email: deployerEmail,
    description: `Deploys BoxLite's ${stage} stage, assumed by GitHub Actions in the ${stage} environment`,
    log,
  })
  await grantProjectRoles({ gcloud, project, email: deployerEmail, roles: DEPLOYER_ROLES, log })
  // One stage, one environment: the same claim the AWS deploy role trusts.
  await allowImpersonation({ gcloud, email: deployerEmail, projectNumber, pool, attribute: 'environment', value: stage })
  log(`    environment ${stage} may act as it`)

  const publisher = 'boxlite-mbuild'
  const publisherEmail = serviceAccountEmail(publisher, project)
  log(`==> ${publisher}`)
  await ensureServiceAccount({
    gcloud,
    id: publisher,
    email: publisherEmail,
    description: "Publishes BoxLite's images, assumed by GitHub Actions",
    log,
  })
  await grantProjectRoles({ gcloud, project, email: publisherEmail, roles: PUBLISHER_ROLES, log })
  /*
   * Both claim shapes, matching mbuild.yml/publish-image.yml's own two
   * consumers: a stage publish declares `environment: <stage>`, a main-branch
   * publish declares none and arrives as a ref. One role serves every stage,
   * so bootstrapping a second GCP stage adds that stage's binding and
   * reapplies the ref one.
   */
  await allowImpersonation({ gcloud, email: publisherEmail, projectNumber, pool, attribute: 'ref', value: 'refs/heads/main' })
  await allowImpersonation({ gcloud, email: publisherEmail, projectNumber, pool, attribute: 'environment', value: stage })
  log(`    refs/heads/main and environment ${stage} may act as it`)

  await ensureRepository({ gcloud, repository, region, log })

  log('')
  log('The state bucket is deliberately not returned; mstage reads it from')
  log(`${BOOTSTRAP_SECRET} and nothing else needs to know its name.`)

  return {
    workloadIdentityProvider: `projects/${projectNumber}/locations/${POOL_LOCATION}/workloadIdentityPools/${pool}/providers/${POOL_PROVIDER}`,
    deployerEmail,
    publisherEmail,
  }
}
