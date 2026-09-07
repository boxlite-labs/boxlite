/*
 * The AWS network: one VPC, a NAT the services egress through, and an internet
 * gateway the runners egress through instead.
 *
 * The two egress paths are the whole shape of this file. Services run in
 * private subnets with no address of their own and reach the internet through
 * `nat: 'ec2'` — an fck-NAT instance, an order of magnitude cheaper than a
 * managed NAT gateway and adequate for the volume a control plane produces.
 * Runners run in *public* subnets with a public IP they only dial out from:
 * they pull box images and their own binary constantly, and routing that
 * through the NAT would put every byte of it through one instance.
 *
 * "Only dial out from" is a property of the security groups rather than of the
 * subnet, so it is arranged here. A runner's one inbound rule admits the VPC's
 * own CIDR on one port, and nothing else — every box port is served inside the
 * runner and never bound on the host's interface, so that single port is the
 * complete surface.
 *
 * The gateway endpoint for S3 is here rather than in the storage module for the
 * same reason the NAT is: it is a route, and routes belong to the network. It
 * also matters more here than elsewhere — every volume read and write from a
 * private task would otherwise cross the NAT instance.
 */

import type { Network, NetworkProvider, NetworkRequest, Placement, WorkloadRole } from '../../network.ts'
import { API_PORT } from '../../api.ts'
import { PROXY_PORT } from '../../edge.ts'
import { RUNNER_PORT } from '../../runners.ts'
import { OTLP_HTTP_PORT } from '../../collector.ts'

/** Every port one workload in this network opens to another. */
const INTERNAL_PORTS = [API_PORT, PROXY_PORT, RUNNER_PORT, OTLP_HTTP_PORT]

export const awsNetworkProvider =
  ({ region }: { region: string }): NetworkProvider =>
  (request: NetworkRequest): Network => {
    const vpc = new sst.aws.Vpc('Vpc', {
      // Off is a stage with no outbound internet at all, which means no image
      // pulls. Supported because a stage may be reachable another way, and
      // named rather than assumed so the deploy that chose it says so.
      nat: request.internetEgress ? 'ec2' : undefined,
      transform: {
        // Named so the console shows which zone an instance is in. SST would
        // otherwise leave three identically-labelled NAT instances.
        natInstance: (args: any, _opts: any, resourceName: any) => {
          const index = resourceName.match(/\d+$/)?.[0] ?? ''
          const availabilityZone = aws.ec2.getSubnetOutput({ id: args.subnetId }).availabilityZone
          args.tags = {
            ...args.tags,
            Name: $interpolate`${$app.name}-${$app.stage}-nat-${index}-${availabilityZone}`,
          }
        },
        elasticIp: (args: any, _opts: any, resourceName: any) => {
          const index = resourceName.match(/\d+$/)?.[0] ?? ''
          args.tags = { ...args.tags, Name: `${$app.name}-${$app.stage}-nat-eip-${index}` }
        },
        natSecurityGroup: (args: any) => {
          args.tags = { ...args.tags, Name: `${$app.name}-${$app.stage}-nat-sg` }
        },
      },
    })

    /*
     * Volume traffic that never touches the NAT.
     *
     * A gateway endpoint is a route rather than an interface, so it costs
     * nothing per hour and nothing per gigabyte — and every private task's S3
     * traffic would otherwise cross the one NAT instance the services share.
     */
    new aws.ec2.VpcEndpoint('S3Gateway', {
      vpcId: vpc.nodes.vpc.id,
      serviceName: `com.amazonaws.${region}.s3`,
      vpcEndpointType: 'Gateway',
      routeTableIds: vpc.nodes.privateRouteTables.apply((tables: any) => tables.map((table: any) => table.id)),
    })

    /*
     * The runner's own group, which is what makes its public address
     * egress-only.
     *
     * Without one it falls back to the VPC's default security group, which
     * admits every port from the whole VPC CIDR. One inbound rule from the VPC
     * itself, on the one port the runner multiplexes its control-plane API, its
     * box proxy and its ssh gateway onto, is the complete surface — and nothing
     * on the internet is in that CIDR.
     */
    const runnerSecurityGroup = new aws.ec2.SecurityGroup('RunnerSecurityGroup', {
      vpcId: vpc.nodes.vpc.id,
      description: 'BoxLite runner: one inbound port from inside the VPC, egress anywhere',
      ingress: [
        {
          protocol: 'tcp',
          fromPort: RUNNER_PORT,
          toPort: RUNNER_PORT,
          cidrBlocks: [vpc.nodes.vpc.cidrBlock],
          description: 'Control plane and box proxy, from inside this network only',
        },
      ],
      egress: [{ protocol: '-1', fromPort: 0, toPort: 0, cidrBlocks: ['0.0.0.0/0'] }],
      tags: { Name: `${$app.name}-${$app.stage}-runner-sg` },
    })

    /*
     * One group for the containerised services, holding the edges they share.
     *
     * Three groups — one per role — was considered and rejected: the API, the
     * proxy and the collector all talk to each other in both directions, so
     * three groups would be six rules describing a mesh that one group states
     * once. The runner is the one workload with genuinely different
     * reachability, and it has its own group above.
     */
    const serviceSecurityGroup = new aws.ec2.SecurityGroup('ServiceSecurityGroup', {
      vpcId: vpc.nodes.vpc.id,
      description: 'BoxLite services: each other on their own ports, and outbound HTTPS',
      egress: [{ protocol: '-1', fromPort: 0, toPort: 0, cidrBlocks: ['0.0.0.0/0'] }],
      tags: { Name: `${$app.name}-${$app.stage}-service-sg` },
    })
    const placed = INTERNAL_PORTS.map(
      (port) =>
        new aws.vpc.SecurityGroupIngressRule(`ServiceIngress${port}`, {
          securityGroupId: serviceSecurityGroup.id,
          referencedSecurityGroupId: serviceSecurityGroup.id,
          ipProtocol: 'tcp',
          fromPort: port,
          toPort: port,
          description: `Service to service on ${port}`,
        }),
    )
    /*
     * The runner answers the API and the proxy, and both are in the service
     * group. Stated as a rule on the runner's group rather than by putting the
     * runner in the service group: a runner must not inherit the services'
     * reachability to the database and the cache.
     */
    const runnerReachable = new aws.vpc.SecurityGroupIngressRule('RunnerIngressFromServices', {
      securityGroupId: runnerSecurityGroup.id,
      referencedSecurityGroupId: serviceSecurityGroup.id,
      ipProtocol: 'tcp',
      fromPort: RUNNER_PORT,
      toPort: RUNNER_PORT,
      description: 'The control plane and the proxy reach a runner',
    })
    // And the other direction: a runner registers itself and ships telemetry.
    const servicesReachable = [API_PORT, OTLP_HTTP_PORT].map(
      (port) =>
        new aws.vpc.SecurityGroupIngressRule(`ServiceIngressFromRunner${port}`, {
          securityGroupId: serviceSecurityGroup.id,
          referencedSecurityGroupId: runnerSecurityGroup.id,
          ipProtocol: 'tcp',
          fromPort: port,
          toPort: port,
          description: `A runner reaches the services on ${port}`,
        }),
    )

    const placementFor = (role: WorkloadRole): Placement =>
      role === 'runner'
        ? {
            cloud: 'aws',
            exposure: 'egress-only-public',
            subnets: vpc.publicSubnets,
            securityGroups: [runnerSecurityGroup.id],
          }
        : {
            cloud: 'aws',
            exposure: 'private',
            subnets: vpc.privateSubnets,
            securityGroups: [serviceSecurityGroup.id],
          }

    return {
      binding: {
        cloud: 'aws',
        vpc,
        vpcId: vpc.nodes.vpc.id,
        privateSubnets: vpc.privateSubnets,
        publicSubnets: vpc.publicSubnets,
        cloudmapNamespaceId: vpc.nodes.cloudmapNamespace.id,
        cloudmapNamespaceName: vpc.nodes.cloudmapNamespace.name,
        cidr: vpc.nodes.vpc.cidrBlock,
      },
      placementFor,
      ready: [...placed, runnerReachable, ...servicesReachable],
    }
  }
