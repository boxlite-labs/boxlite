/*
 * Nothing.
 *
 * Cloud Run has no cluster: a service names a region and a service account, and
 * the placement already carries both. This provider therefore builds no
 * resource, and that is the honest answer rather than a gap — inventing a GKE
 * cluster to make the shapes match would cost money every hour and explain
 * nothing about how anything runs.
 *
 * It exists at all because the stack asks every cloud the same question, and a
 * bundle with a hole in it would push the difference up into the composition
 * root, where it would have to know which cloud it was on. The file is short
 * because the difference is real, not because it was skipped.
 */

import type { Cluster, ClusterProvider, ContainerRole, WorkloadHost } from '../../cluster.ts'
import type { Network } from '../../network.ts'

export const gcpClusterProvider =
  ({ region, network }: { region: string; network: Network }): ClusterProvider =>
  (): Cluster => {
    const host: WorkloadHost = { cloud: 'gcp', region }
    return {
      hostFor: (_role: ContainerRole) => host,
      /*
       * The network's rules all the same.
       *
       * Nothing here creates them, but every workload still has to wait for
       * them — a Cloud Run revision placed before its firewall rule exists
       * starts, fails to reach what it needs, and is replaced in a loop. The
       * AWS cluster carries the same list for the same reason; this is the one
       * thing this module does even though it builds nothing.
       */
      ready: network.ready,
    }
  }
