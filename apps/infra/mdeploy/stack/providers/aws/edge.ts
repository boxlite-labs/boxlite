/*
 * The box proxy, behind a network load balancer with a wildcard certificate.
 *
 * A network load balancer rather than an application one, and a `443/tls`
 * listener rather than `443/https`: the proxy terminates TLS itself so it can
 * read the SNI name — `<port>-<boxid>.<domain>` — before it has anywhere to
 * forward to. An application load balancer would have terminated first and lost
 * the name the routing decision depends on.
 *
 * The topology is protected: the load balancer, the listener and the target
 * group all carry `protect`. A replacement that swapped the listener before ECS
 * had attached the new target group would take every running box offline, and a
 * routine task revision never needs one. Protecting them makes an immutable
 * change fail loudly instead of half-switching.
 */

import type { WorkloadHost } from '../../cluster.ts'
import type { Edge, EdgeProvider, EdgeRequest } from '../../edge.ts'
import { PROXY_PORT } from '../../edge.ts'
import type { Placement } from '../../network.ts'

export const awsEdgeProvider =
  ({
    host,
    placement,
    dns,
    dependsOn,
  }: {
    host: Extract<WorkloadHost, { cloud: 'aws' }>
    placement: Extract<Placement, { cloud: 'aws' }>
    dns: ReturnType<typeof sst.cloudflare.dns>
    dependsOn: any[]
  }): EdgeProvider =>
  (request: EdgeRequest): Edge => {
    const service = new sst.aws.Service(
      'Proxy',
      {
        cluster: host.cluster,
        image: request.image,
        wait: true,
        loadBalancer: {
          domain: {
            name: request.domain,
            // One certificate for every box that will ever exist. Without it a
            // new box is unreachable until someone issues a certificate for it.
            aliases: [`*.${request.domain}`],
            dns,
          },
          rules: [{ listen: '443/tls', forward: `${PROXY_PORT}/tcp` }],
        },
        environment: {
          ...request.environment,
          PROXY_PORT: String(PROXY_PORT),
          PROXY_PROTOCOL: request.protocol,
          // api-client-go appends paths like `/config` directly, so the `/api`
          // prefix belongs here rather than at every call site inside the proxy.
          BOXLITE_API_URL: $interpolate`${request.apiUrl}/api`,
        },
        ssm: request.secrets,
        transform: {
          loadBalancer: (args: any, opts: any) => {
            args.loadBalancerType = 'network'
            opts.protect = true
          },
          listener: (_args: any, opts: any) => {
            opts.protect = true
          },
          target: (args: any, opts: any) => {
            // A TCP listener has no health notion of its own, so the target
            // group is where the proxy's own `/health` route is probed.
            args.healthCheck = {
              enabled: true,
              protocol: 'HTTP',
              path: '/health',
              port: 'traffic-port',
              matcher: '200-399',
              interval: 30,
              timeout: 5,
              healthyThreshold: 2,
              unhealthyThreshold: 3,
            }
            opts.protect = true
          },
          service: (args: any) => {
            args.networkConfiguration = {
              ...args.networkConfiguration,
              subnets: placement.subnets,
              securityGroups: placement.securityGroups,
              assignPublicIp: false,
            }
          },
        },
      },
      { dependsOn },
    )

    return {
      url: $util.output(`https://${request.domain}`),
      metricTarget: service.nodes.loadBalancer.arnSuffix,
      ready: [service],
    }
  }
