/*
 * One ECS cluster, shared by the three containerised roles.
 *
 * One rather than three, which is where this differs from the console this
 * pattern came from. There, the three roles reach genuinely different things
 * and a shared cluster would have carried the union of their reachability.
 * Here reachability is expressed by the security groups the network module
 * hands out, not by the cluster — so three clusters would be three copies of
 * one object, differing in nothing, and each an extra thing to name in a
 * targeted deploy.
 *
 * The `vpc` argument is passed as a plain object rather than as the component,
 * which is SST's documented escape hatch and the only way to say what BoxLite
 * needs: tasks in the private subnets with no public address, load balancers in
 * the public ones. Handing over the component instead puts Fargate tasks in
 * public subnets with public IPs, which is its default.
 */

import type { Cluster, ClusterProvider, ContainerRole, WorkloadHost } from '../../cluster.ts'
import type { Network, NetworkBinding } from '../../network.ts'

export const awsClusterProvider =
  ({ network }: { network: Network }): ClusterProvider =>
  (): Cluster => {
    const binding = network.binding as Extract<NetworkBinding, { cloud: 'aws' }>
    const cluster = new sst.aws.Cluster('Cluster', {
      forceUpgrade: 'v2',
      vpc: {
        id: binding.vpcId,
        securityGroups: binding.vpc.securityGroups,
        containerSubnets: binding.privateSubnets,
        loadBalancerSubnets: binding.publicSubnets,
        cloudmapNamespaceId: binding.cloudmapNamespaceId,
        cloudmapNamespaceName: binding.cloudmapNamespaceName,
      },
    })

    const host: WorkloadHost = { cloud: 'aws', cluster }
    return {
      hostFor: (_role: ContainerRole) => host,
      /*
       * The network's rules, carried through the cluster.
       *
       * Every workload waits on these, and this is the one place that says so.
       * A task placed before the rules exist starts, fails to pull its image or
       * to reach the API, and is replaced in a loop that reports as a broken
       * image rather than as a missing rule.
       */
      ready: network.ready,
    }
  }
