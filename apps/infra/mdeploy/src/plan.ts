/*
 * Which modules a deploy is made of, what each one builds, and what it needs
 * first.
 *
 * One deploy used to be one opaque command — `deployStack()` in
 * `apps/infra/stack/deploy.ts`, three hundred lines that build a VPC and end by
 * booting an EC2 runner. Split into modules it becomes a sequence a person can
 * read, resume and reason about: a failure names the module that failed rather
 * than a resource somewhere inside an hour-long run, and two modules that need
 * nothing from each other run at the same time.
 *
 * The dependencies below are the same ones each provider already expresses by
 * taking another module's handle as an argument — `awsClusterProvider({
 * network })` cannot run before the network exists, and says so in its
 * signature. This file states them once in a form a workflow can read, and the
 * test beside it asserts the workflow's own graph matches. Two hand-written
 * copies of a dependency graph is one copy too many.
 *
 * `components` are SST's logical names, which is what `--target` selects on.
 * They are the names the incumbent stack already uses — `Vpc`, `Database`,
 * `Cache`, `Storage`, `Cluster`, `Api`, `Proxy`, `ApiCdn`, `OtelCollector`,
 * `Mail`, `Runner` — so a targeted deploy adopts existing resources rather than
 * creating a second set beside them. That is what makes the cutover a diff to
 * read rather than a migration to perform.
 */

export type ModuleName =
  | 'network'
  | 'storage'
  | 'database'
  | 'cache'
  | 'cluster'
  | 'clickhouse'
  | 'mail'
  | 'observability'
  | 'api'
  | 'edge'
  | 'runners'
  | 'alarms'

export type ModulePlan = {
  /** SST component names, passed to one comma-separated `--target`. */
  components: string[]
  /** Modules that must be deployed first. */
  needs: ModuleName[]
  /** Why it waits, for the person reading a failed run. */
  because: string
}

export const PLAN: Record<ModuleName, ModulePlan> = {
  network: {
    components: ['Vpc', 'S3Gateway'],
    needs: [],
    because: 'nothing can be placed before there is somewhere to place it',
  },
  storage: {
    components: ['Storage'],
    needs: [],
    // Deliberately not after the network: a bucket is reached over the API, and
    // the gateway endpoint that keeps that traffic off the NAT belongs to the
    // network's own module rather than to the bucket's.
    because: 'a bucket is reached over the API, not from inside the network',
  },
  database: {
    components: ['Database'],
    needs: ['network'],
    because: 'a private address has to come from somewhere',
  },
  cache: {
    components: ['Cache'],
    needs: ['network'],
    because: 'the same, and it is reachable from the same private subnets',
  },
  cluster: {
    components: ['Cluster'],
    needs: ['network'],
    because: 'tasks are placed in the subnets and security groups the network arranged',
  },
  clickhouse: {
    components: ['ClickHouse', 'ClickHouseWriterSecret', 'ClickHouseReaderSecret'],
    needs: ['network'],
    // Self-hosted ClickHouse is one instance on a private address; a managed
    // one is an endpoint this module only records. Either way it is telemetry
    // storage, and nothing that serves a request waits on it.
    because: 'the single-node backend lives on a private address inside this network',
  },
  mail: {
    components: ['Mail'],
    needs: [],
    // SES verification blocks on DNS rather than on anything this stack builds,
    // so it starts as early as it can and the API waits for the result.
    because: 'a domain identity is verified against DNS, not against this stack',
  },
  observability: {
    components: ['OtelCollector'],
    needs: ['cluster', 'clickhouse'],
    because: 'the collector runs in the cluster and writes to ClickHouse',
  },
  api: {
    components: ['Api', 'ApiCdn', 'S3AccessRole'],
    needs: ['database', 'cache', 'storage', 'observability', 'mail'],
    because: 'it queries tables, vends bucket credentials, emits OTLP and sends invitations',
  },
  edge: {
    components: ['Proxy'],
    needs: ['api'],
    because: 'the proxy resolves a box through the API before it forwards a byte',
  },
  runners: {
    components: ['Runner', 'RunnerRole', 'RunnerProfile', 'RunnerSecurityGroup'],
    needs: ['api'],
    because: 'a runner registers itself with the control plane at first boot',
  },
  alarms: {
    components: ['ApiServerErrorAlarm', 'ProxyUnhealthyTargetAlarm', 'RunnerUnreachableAlarm'],
    needs: ['edge', 'runners'],
    // Last on purpose: one that cannot be created must not roll back a service
    // that is already serving.
    because: 'an alarm watches something that has to exist first',
  },
}

export const MODULE_NAMES = Object.keys(PLAN) as ModuleName[]

/**
 * The modules deployed together, in the order the batches run.
 *
 * Modules that need nothing from each other go into one apply, which is one
 * state write and therefore safe — and Pulumi runs the independent resources
 * inside it concurrently, which is the parallelism a chain of separate applies
 * gives up. Batching recovers it without ever having two processes write
 * `app/<app>/<stage>.json` at once.
 *
 * A batch is a level of the dependency graph, so the grouping is derived rather
 * than chosen: two modules share a batch exactly when neither can reach the
 * other. Adding a dependency splits a batch by itself.
 *
 * Serial between batches, and not as a simplification. One app and stage share
 * a single state object, so two applies against the same stage read and write
 * the same file and the second to finish erases what the first recorded.
 * Nothing about `--target` changes that: the target selects what to apply, not
 * which state to write.
 */
export const deployBatches = (): ModuleName[][] => {
  const done = new Set<ModuleName>()
  const batches: ModuleName[][] = []
  while (done.size < MODULE_NAMES.length) {
    const ready = MODULE_NAMES.filter((name) => !done.has(name) && PLAN[name].needs.every((need) => done.has(need)))
    if (ready.length === 0) throw new Error('The module plan has a cycle; no module is ready')
    batches.push(ready)
    for (const name of ready) done.add(name)
  }
  return batches
}

/** What one batch names as its job, and as its `--module` argument. */
export const batchName = (batch: ModuleName[]): string => batch.join('-')
