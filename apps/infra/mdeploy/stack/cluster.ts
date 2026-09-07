/*
 * Somewhere for each containerised workload to run.
 *
 * This is the module where the two clouds disagree most, and the contract says
 * so rather than hiding it. ECS needs a cluster before it can place a task, and
 * the cluster is where the network placement and the service-discovery
 * namespace are fixed. Cloud Run has no cluster: a service names a region and a
 * service account and that is the whole of it, both of which the placement
 * already carries. The GCP member of `WorkloadHost` is therefore almost empty,
 * and inventing a GKE cluster to make the shapes match would cost money and
 * explain nothing.
 *
 * The runner is not here. It is a virtual machine with nested KVM rather than a
 * container, so it has no host to be placed in on either cloud — it *is* the
 * host. `runners.ts` takes a placement straight from the network for that
 * reason, and asking this module for one would have meant a cluster with no
 * tasks in it on AWS and nothing at all on GCP.
 */

import type { WorkloadRole } from './network.ts'

/** The roles this module can be asked for: everything except the runner. */
export type ContainerRole = Exclude<WorkloadRole, 'runner'>

export type ClusterRequest = {
  /** The roles that need somewhere to run. */
  roles: readonly ContainerRole[]
}

/**
 * What a workload is deployed into. On AWS the cluster itself, because every
 * service names one; on GCP the region, because a Cloud Run service names that
 * instead and nothing else about a "host" exists to name.
 */
export type WorkloadHost = { cloud: 'aws'; cluster: CloudResource } | { cloud: 'gcp'; region: string }

export type Cluster = {
  hostFor: (role: ContainerRole) => WorkloadHost
  ready: any[]
}

export type ClusterProvider = (request: ClusterRequest) => Cluster
