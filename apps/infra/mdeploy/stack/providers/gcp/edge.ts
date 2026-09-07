/*
 * The box proxy on GCP, and the one module that could not be a Cloud Run
 * service.
 *
 * Everything else BoxLite runs in a container is a Cloud Run service here. The
 * proxy is not, and the reason is worth stating plainly rather than working
 * around: the proxy terminates TLS itself so it can read the SNI name —
 * `<port>-<boxid>.<domain>` — and decide which runner holds that box. That
 * needs a layer-4 path from the client to the container.
 *
 * Google offers exactly one load balancer that does not terminate TLS on the
 * way in: the regional external *passthrough* network load balancer. Cloud Run
 * cannot be a backend of one — its only external front ends are the HTTPS load
 * balancer, which terminates, and its own `run.app` endpoint, which also
 * terminates. So the proxy runs on a managed instance group of
 * container-optimised VMs, and the passthrough balancer forwards to it.
 *
 * That is a genuine asymmetry with the AWS side, where an NLB with a `443/tls`
 * listener forwards to a Fargate task and the workload stays a container in a
 * cluster. It costs a machine per zone that the AWS side does not spend, and it
 * is written down here rather than hidden behind a shape that pretends the two
 * clouds answered the same way.
 *
 * The wildcard certificate is the proxy's own, not the balancer's — a
 * passthrough balancer has no certificate, because it never reads the
 * handshake. The proxy container obtains and renews it, which is why nothing
 * here provisions one.
 */

import type { Edge, EdgeProvider, EdgeRequest } from '../../edge.ts'
import { PROXY_PORT } from '../../edge.ts'
import type { Placement } from '../../network.ts'
import { splitSecretRef } from './secret-env.ts'

/**
 * Container-Optimized OS, which runs a container declared in instance metadata.
 *
 * The alternative was a general image with a startup script that installs
 * Docker, which is the same thing done worse: slower to boot, and one more
 * thing to keep patched on a host that faces the internet.
 */
const COS_IMAGE = 'cos-cloud/cos-stable'

/** What the proxy runs on. Small: it forwards bytes, it does not compute. */
const MACHINE_TYPE = 'e2-standard-2'

/**
 * The container declaration Container-Optimized OS reads.
 *
 * Written as the `gce-container-declaration` metadata value, which is the
 * documented contract for this image family. Secrets arrive the long way round
 * — fetched by the startup script and written to a file the container reads —
 * because a metadata value is readable by anything on the host, and a proxy is
 * a host that faces the internet.
 */
const containerDeclaration = (image: string, environment: Record<string, string>): string =>
  [
    'spec:',
    '  containers:',
    '    - name: proxy',
    `      image: ${image}`,
    '      securityContext:',
    '        privileged: false',
    '      stdin: false',
    '      tty: false',
    '      env:',
    ...Object.entries(environment).map(([name, value]) => `        - name: ${name}\n          value: '${value}'`),
    '  restartPolicy: Always',
  ].join('\n')

export const gcpEdgeProvider =
  ({
    project,
    region,
    placement,
    zoneId,
    dependsOn,
  }: {
    project: string
    region: string
    placement: Extract<Placement, { cloud: 'gcp' }>
    /** The Cloudflare zone the two records are written into. */
    zoneId: string
    dependsOn: any[]
  }): EdgeProvider =>
  (request: EdgeRequest): Edge => {
    const prefix = `${$app.name}-${$app.stage}`

    /*
     * Every secret the proxy reads, fetched at boot and written where the
     * container can read it. Not into the container declaration: metadata is
     * readable by anything on this host.
     */
    const secretNames = Object.keys(request.secrets)
    const startupScript =
      secretNames.length === 0
        ? ''
        : $resolve(Object.values(request.secrets)).apply((references: string[]) => {
            const fetches = secretNames.map((name, index) => {
              const { secret, version } = splitSecretRef(references[index] as string)
              return `printf '%s=%s\\n' ${name} "$(gcloud secrets versions access ${version} --secret=${secret} --format='get(payload.data)' | base64 -d)" >> /run/proxy.env`
            })
            return ['#!/bin/bash', 'set -euo pipefail', ': > /run/proxy.env', 'chmod 600 /run/proxy.env', ...fetches].join('\n')
          })

    const template = new gcp.compute.InstanceTemplate('ProxyTemplate', {
      namePrefix: `${prefix}-proxy-`,
      project,
      region,
      machineType: MACHINE_TYPE,
      disks: [{ sourceImage: COS_IMAGE, autoDelete: true, boot: true, diskSizeGb: 20 }],
      networkInterfaces: [
        {
          subnetwork: placement.subnetwork,
          // A passthrough balancer forwards to the instance's own address, so
          // the instance needs an external one. Ingress is the firewall's to
          // bound, and only 443 is opened below.
          accessConfigs: [{}],
        },
      ],
      serviceAccount: { email: placement.serviceAccount, scopes: ['cloud-platform'] },
      metadata: {
        'gce-container-declaration': $resolve([request.image, request.apiUrl]).apply(([image, apiUrl]) =>
          containerDeclaration(image, {
            ...(request.environment as Record<string, string>),
            PROXY_PORT: String(PROXY_PORT),
            PROXY_PROTOCOL: request.protocol,
            // api-client-go appends paths like `/config` directly, so the
            // `/api` prefix belongs here rather than inside the proxy.
            BOXLITE_API_URL: `${String(apiUrl).replace(/\/$/, '')}/api`,
            PROXY_DOMAIN: request.domain,
          }),
        ),
        ...(startupScript ? { 'startup-script': startupScript } : {}),
      },
      // A template is immutable, so a change makes a new one and the group
      // rolls onto it rather than failing on an in-place update.
      lifecycle: { createBeforeDestroy: true },
    })

    const health = new gcp.compute.RegionHealthCheck('ProxyHealthCheck', {
      name: `${prefix}-proxy`,
      project,
      region,
      // The proxy's own route, on its own port. A TCP check on 443 would call a
      // host healthy while the container behind it was still starting.
      httpHealthCheck: { requestPath: '/health', port: PROXY_PORT },
      checkIntervalSec: 30,
      timeoutSec: 5,
      healthyThreshold: 2,
      unhealthyThreshold: 3,
    })

    const group = new gcp.compute.RegionInstanceGroupManager(
      'Proxy',
      {
        name: `${prefix}-proxy`,
        project,
        region,
        baseInstanceName: `${prefix}-proxy`,
        versions: [{ instanceTemplate: template.selfLinkUnique }],
        targetSize: 2,
        namedPorts: [{ name: 'tls', port: 443 }],
        // Rolling, one at a time, with a spare: every running box's connection
        // goes through these, so a group that replaced both at once would drop
        // every session.
        updatePolicy: {
          type: 'PROACTIVE',
          minimalAction: 'REPLACE',
          maxSurgeFixed: 1,
          maxUnavailableFixed: 0,
        },
        // The same check the balancer uses. A group with no autohealing keeps
        // a host that stopped answering in rotation until someone notices.
        autoHealingPolicies: { healthCheck: health.id, initialDelaySec: 300 },
      },
      { dependsOn },
    )

    const backend = new gcp.compute.RegionBackendService('ProxyBackend', {
      name: `${prefix}-proxy`,
      project,
      region,
      // Passthrough: the balancer forwards the connection without reading the
      // handshake, which is the whole reason this module is not a Cloud Run
      // service. See the note at the top.
      loadBalancingScheme: 'EXTERNAL',
      protocol: 'TCP',
      healthChecks: [health.id],
      backends: [{ group: group.instanceGroup }],
    })

    const address = new gcp.compute.Address('ProxyAddress', { name: `${prefix}-proxy`, project, region })
    const forwarding = new gcp.compute.ForwardingRule('ProxyForwardingRule', {
      name: `${prefix}-proxy`,
      project,
      region,
      loadBalancingScheme: 'EXTERNAL',
      ipProtocol: 'TCP',
      ports: ['443'],
      ipAddress: address.address,
      backendService: backend.id,
    })

    // 443 from anywhere, and nothing else. This is the one workload in the
    // stack that is meant to be reachable from the internet.
    const firewall = new gcp.compute.Firewall('ProxyFirewall', {
      name: `${prefix}-proxy`,
      project,
      network: placement.subnetwork.apply((subnetwork: string) => subnetwork.replace(/\/regions\/.*$/, '')),
      direction: 'INGRESS',
      allows: [{ protocol: 'tcp', ports: ['443'] }],
      sourceRanges: ['0.0.0.0/0'],
      targetServiceAccounts: [placement.serviceAccount],
    })

    /*
     * Two records, not one. The apex is what a client resolves for the proxy
     * itself; the wildcard is every box that will ever exist. Both unproxied:
     * Cloudflare's proxy terminates TLS, which would take the SNI name this
     * whole module exists to read.
     */
    const apex = new cloudflare.Record('ProxyRecord', {
      zoneId,
      name: request.domain,
      type: 'A',
      content: address.address,
      proxied: false,
      ttl: 60,
    })
    const wildcard = new cloudflare.Record('ProxyWildcardRecord', {
      zoneId,
      name: `*.${request.domain}`,
      type: 'A',
      content: address.address,
      proxied: false,
      ttl: 60,
    })

    return {
      url: $util.output(`https://${request.domain}`),
      // A log-based metric filters on the group's own name here, where AWS
      // dimensions a metric by the balancer's ARN suffix.
      metricTarget: group.name,
      ready: [group, forwarding, firewall, apex, wildcard],
    }
  }
