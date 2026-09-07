/*
 * The control plane as a Cloud Run service, behind a global load balancer.
 *
 * The load balancer is what makes the two front doors of the AWS side one door
 * here. There, a CDN serves the dashboard's assets and the load balancer
 * answers `api.<domain>` directly, because CloudFront caps a WebSocket at ten
 * minutes. Google's global load balancer has no such cap and its CDN is a flag
 * on the same backend, so `url` and `address` are the same hostname — which the
 * contract already allows for by having both.
 *
 * Capabilities become IAM bindings rather than policy documents, and they are
 * attached at the *resource* rather than at the principal. That is the shape of
 * Google's IAM and not a choice: there is nothing to hand a service account.
 * The one that needs care is the volume-bucket grant — Google has no wildcard
 * over resource names, so the role is granted at the project with a CEL
 * condition that puts the prefix back. Without that condition the API could
 * delete any bucket in the project, which is why `storage.ts` carries the
 * condition rather than leaving it to be written here.
 */

import type { Api, ApiCapability, ApiDependencies, ApiProvider, ApiRequest } from '../../api.ts'
import { CACHE_PASSWORD_VARIABLE } from '../../cache.ts'
import { CLICKHOUSE_PASSWORD_VARIABLE } from '../../clickhouse.ts'
import { DATABASE_PASSWORD_VARIABLE } from '../../database.ts'
import type { Placement } from '../../network.ts'
import type { StorageBinding } from '../../storage.ts'
import { containerEnvironment, secretIdOf } from './secret-env.ts'
import { VOLUME_OBJECT_ACCESS_ROLE } from './storage.ts'

const onGcp = (storage: { binding: StorageBinding }): Extract<StorageBinding, { cloud: 'gcp' }> => {
  if (storage.binding.cloud !== 'gcp') throw new Error(`The GCP API was handed ${storage.binding.cloud} storage`)
  return storage.binding
}

/** One capability, as the bindings it becomes. Each returns its own resources. */
const bindingsFor = ({
  capability,
  index,
  project,
  member,
  bucketName,
}: {
  capability: ApiCapability
  index: number
  project: string
  member: $util.Output<string>
  bucketName: $util.Output<string>
}): any[] => {
  const principal = member.apply((email: string) => `serviceAccount:${email}`)
  switch (capability.kind) {
    case 'list-own-bucket':
      // On the bucket itself, which needs no condition: it already names
      // exactly one resource.
      return [
        new gcp.storage.BucketIAMMember(`ApiCapability${index}`, {
          bucket: bucketName,
          role: onGcp(capability.storage).listGrant,
          member: principal,
        }),
      ]
    case 'manage-volume-buckets': {
      const storage = onGcp(capability.storage)
      return [
        new gcp.projects.IAMMember(`ApiCapability${index}`, {
          project,
          role: storage.lifecycleGrant,
          member: principal,
          // The prefix, as this cloud is able to express it. See the note above.
          condition: storage.volumeCondition,
        }),
      ]
    }
    case 'vend-volume-credentials':
      // Google's equivalent of assuming a role: the API mints a short-lived
      // token for the vending account and scopes it to one organization.
      return [
        new gcp.serviceaccount.IAMMember(`ApiCapability${index}`, {
          serviceAccountId: onGcp(capability.storage).credentialVending.serviceAccount.apply(
            (email: string) => `projects/${project}/serviceAccounts/${email}`,
          ),
          role: 'roles/iam.serviceAccountTokenCreator',
          member: principal,
        }),
      ]
    case 'read-telemetry':
      // The reader password reaches the container by reference, so the account
      // has to be allowed to resolve that reference. Named per secret rather
      // than granted at the project: nothing else in Secret Manager.
      return capability.clickhouse.active
        ? [
            new gcp.secretmanager.SecretIamMember(`ApiCapability${index}`, {
              project,
              secretId: capability.clickhouse.reader.passwordRef.apply((reference: string) =>
                secretIdOf(reference),
              ),
              role: 'roles/secretmanager.secretAccessor',
              member: principal,
            }),
          ]
        : []
    case 'read-secret':
      return [
        new gcp.secretmanager.SecretIamMember(`ApiCapability${index}`, {
          project,
          secretId: capability.ref.apply((reference: string) => secretIdOf(reference)),
          role: 'roles/secretmanager.secretAccessor',
          member: principal,
        }),
      ]
  }
}

export const gcpApiProvider =
  ({
    dependencies,
    project,
    region,
    domain,
    callers,
    zoneId,
  }: {
    dependencies: ApiDependencies
    project: string
    region: string
    /** The hostname the dashboard and the SDKs reach it on. */
    domain: string
    /** The identities allowed to invoke it. The proxy and the runner. */
    callers: $util.Output<string>[]
    /** The Cloudflare zone the record is written into. */
    zoneId: string
  }): ApiProvider =>
  (request: ApiRequest): Api => {
    const prefix = `${$app.name}-${$app.stage}`
    const placement = dependencies.placement as Extract<Placement, { cloud: 'gcp' }>
    const storage = onGcp(dependencies.storage)

    const service = new gcp.cloudrunv2.Service(
      'Api',
      {
        name: `${prefix}-api`,
        project,
        location: region,
        // The load balancer below is the only thing that reaches it from
        // outside, and it enters through Google's own front end.
        ingress: 'INGRESS_TRAFFIC_INTERNAL_LOAD_BALANCER',
        template: {
          serviceAccount: placement.serviceAccount,
          vpcAccess: { egress: 'PRIVATE_RANGES_ONLY', networkInterfaces: [{ subnetwork: placement.subnetwork }] },
          containers: [
            {
              image: request.image,
              ports: [{ name: 'http1', containerPort: request.port }],
              startupProbe: { httpGet: { path: '/api/health', port: request.port }, failureThreshold: 30 },
              livenessProbe: { httpGet: { path: '/api/health', port: request.port } },
              envs: containerEnvironment({
                values: request.environment,
                addresses: {
                  ...request.secrets,
                  [DATABASE_PASSWORD_VARIABLE]: dependencies.database.binding.passwordRef,
                  [CACHE_PASSWORD_VARIABLE]: dependencies.cache.binding.passwordRef,
                  ...(dependencies.clickhouse.active
                    ? { [CLICKHOUSE_PASSWORD_VARIABLE]: dependencies.clickhouse.reader.passwordRef }
                    : {}),
                },
              }),
            },
          ],
        },
      },
      { dependsOn: dependencies.waitFor },
    )

    const invokers = callers.map(
      (member, index) =>
        new gcp.cloudrunv2.ServiceIamMember(`ApiInvoker${index}`, {
          project,
          location: region,
          name: service.name,
          role: 'roles/run.invoker',
          member: member.apply((email: string) => `serviceAccount:${email}`),
        }),
    )

    /*
     * What a vended token may do inside one volume bucket.
     *
     * The mirror of the AWS side's `S3AccessRolePolicy`: this is the ceiling,
     * and the per-organization condition the API attaches when it mints a token
     * narrows it further. Effective access is the intersection.
     */
    const vendingCeiling = new gcp.projects.IAMMember('VolumeAccessCeiling', {
      project,
      role: VOLUME_OBJECT_ACCESS_ROLE,
      member: storage.credentialVending.serviceAccount.apply((email: string) => `serviceAccount:${email}`),
      condition: storage.volumeCondition,
    })

    const granted = request.capabilities.flatMap((capability, index) =>
      bindingsFor({ capability, index, project, member: placement.serviceAccount, bucketName: dependencies.storage.name }),
    )

    /*
     * The global load balancer: a serverless network endpoint group pointing at
     * the service, a backend, a URL map, a managed certificate and a forwarding
     * rule. Five resources for what an ALB does in one, which is simply how
     * this cloud spells it.
     */
    const endpointGroup = new gcp.compute.RegionNetworkEndpointGroup('ApiEndpointGroup', {
      name: `${prefix}-api-neg`,
      project,
      region,
      networkEndpointType: 'SERVERLESS',
      cloudRun: { service: service.name },
    })
    const backend = new gcp.compute.BackendService('ApiBackend', {
      name: `${prefix}-api`,
      project,
      loadBalancingScheme: 'EXTERNAL_MANAGED',
      protocol: 'HTTPS',
      backends: [{ group: endpointGroup.id }],
      // An hour, to match `apps/api`'s own keep-alive: an exec attach that idles
      // through a pause must not be closed under it.
      timeoutSec: 3_600,
    })
    const urlMap = new gcp.compute.URLMap('ApiUrlMap', {
      name: `${prefix}-api`,
      project,
      defaultService: backend.id,
    })
    const certificate = new gcp.compute.ManagedSslCertificate('ApiCertificate', {
      name: `${prefix}-api`,
      project,
      managed: { domains: [domain] },
    })
    const proxy = new gcp.compute.TargetHttpsProxy('ApiHttpsProxy', {
      name: `${prefix}-api`,
      project,
      urlMap: urlMap.id,
      sslCertificates: [certificate.id],
    })
    const address = new gcp.compute.GlobalAddress('ApiAddress', { name: `${prefix}-api`, project })
    const forwarding = new gcp.compute.GlobalForwardingRule('ApiForwardingRule', {
      name: `${prefix}-api`,
      project,
      target: proxy.id,
      portRange: '443',
      ipAddress: address.address,
      loadBalancingScheme: 'EXTERNAL_MANAGED',
    })

    /*
     * The one DNS record, written into Cloudflare rather than Cloud DNS.
     *
     * The zone is not Google's on either cloud, which is the one part of the
     * front door that did not have to be replaced. Proxying is off: Google's
     * managed certificate is validated by reaching this address directly, and a
     * proxied record answers from Cloudflare instead — so the certificate would
     * never leave `PROVISIONING`.
     */
    const record = new cloudflare.Record('ApiRecord', {
      zoneId,
      name: domain,
      type: 'A',
      content: address.address,
      proxied: false,
      ttl: 60,
    })

    return {
      url: $util.output(`https://${domain}`),
      address: $util.output(`https://${domain}`),
      identity: placement.serviceAccount,
      // A log-based metric filters on the service's own name, which is what
      // this cloud's monitoring knows it by.
      metricTarget: service.name,
      ready: [service, ...invokers, ...granted, vendingCeiling, forwarding, record],
    }
  }
