/*
 * The control plane: the one workload that answers a request from outside.
 *
 * What it may do is described as capabilities rather than as policy. *Vend a
 * credential for one box's volume* is one sentence on both clouds; what it
 * expands into is not. On AWS it becomes an `sts:AssumeRole` on a named role
 * whose trust policy names this task role, plus the bucket-lifecycle actions
 * the API demonstrably calls and no others — deliberately not `s3:*`, because
 * that tail is what would let a compromised control plane make a customer's
 * volume public. On GCP the same capability is a `serviceAccountTokenCreator`
 * binding and two storage roles.
 *
 * Writing them as capabilities is what keeps the expansion in one file per
 * cloud. The alternative — a policy document in the shared module — would name
 * an ARN, which is the point at which a module stops being portable.
 *
 * `secrets` is separate from `environment` and is not a convenience. A value in
 * `environment` reaches the container as plain text that anyone who can
 * describe the task definition or the revision can read, and every revision
 * ever registered keeps its own copy. A name in `secrets` is an *address* the
 * platform resolves just before the container starts, so the value never enters
 * the deploy, the task definition, or the revision at all.
 */

import type { Cache } from './cache.ts'
import type { ClickHouse } from './clickhouse.ts'
import type { Database } from './database.ts'
import type { Placement } from './network.ts'
import type { Storage } from './storage.ts'
import type { WorkloadHost } from './cluster.ts'

/**
 * What the API is allowed to do, said once and expanded per cloud.
 *
 * A closed union rather than free-form strings: a capability nobody expanded
 * should be a type error in the provider bundle, not a permission that silently
 * did nothing until the first request that needed it.
 */
export type ApiCapability =
  | { kind: 'list-own-bucket'; storage: Storage }
  | { kind: 'manage-volume-buckets'; storage: Storage }
  | { kind: 'vend-volume-credentials'; storage: Storage }
  | { kind: 'read-telemetry'; clickhouse: ClickHouse }
  | { kind: 'read-secret'; ref: $util.Output<string> }

export type ApiRequest = {
  image: $util.Input<string>
  port: number
  /** The hostname the dashboard and the SDKs reach it on. */
  domain: string
  /** Values the container reads, already assembled. */
  environment: Record<string, $util.Input<string>>
  /** Names it reads by reference. See the note above. */
  secrets: Record<string, $util.Input<string>>
  capabilities: ApiCapability[]
}

/** What the API needs to exist before it can be placed. */
export type ApiDependencies = {
  host: WorkloadHost
  placement: Placement
  database: Database
  cache: Cache
  storage: Storage
  /**
   * The telemetry store it reads back, or the disabled handle.
   *
   * Here as well as in `capabilities` because the two say different things: a
   * capability is what the API is *allowed* to do, and this is what it is
   * *handed*. The reader password reaches the container by reference, and the
   * grant to resolve that reference is the capability — a stage with either and
   * not the other has an API that either cannot read its own telemetry or holds
   * a permission it never uses.
   */
  clickhouse: ClickHouse
  /** Everything that has to be reconciled before a task is placed. */
  waitFor: any[]
}

export type Api = {
  /** The origin the dashboard is served from, with no trailing slash. */
  url: $util.Output<string>
  /** Where the proxy and the runner reach it, which is not always the same. */
  address: $util.Output<string>
  /**
   * The identity the API runs as, which the storage module's credential
   * vending has to name in a trust policy. An Output because it does not exist
   * until the service does — and the role it names cannot be created before
   * this, which is why the composition root declares that role's *name* up
   * front and its resource afterwards.
   */
  identity: $util.Output<string>
  /**
   * What this cloud's monitoring knows the API by.
   *
   * Cloud-neutral by name and not by value: on AWS it is the load balancer's
   * ARN suffix, which is how CloudWatch dimensions a metric; on GCP it is the
   * Cloud Run service's name, which is how a log-based metric filters one. The
   * alarms module carries it through without reading it, so neither cloud's
   * spelling reaches a shared file — and neither has to derive it from a URL,
   * which is where it would silently stop matching.
   */
  metricTarget: $util.Output<string>
  ready: any[]
}

export type ApiProvider = (request: ApiRequest) => Api

/** The port the container listens on. The same on both clouds. */
export const API_PORT = 3000
