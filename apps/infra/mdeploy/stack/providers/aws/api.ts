/*
 * The control plane on ECS, behind an application load balancer and a CDN.
 *
 * Two front doors, and both are deliberate. The CDN serves the dashboard's
 * static assets at the root domain; the load balancer answers `api.<domain>`
 * directly, because CloudFront caps a WebSocket at ten minutes and times an
 * origin read out at sixty seconds — which breaks `/attach`, build-log
 * streaming and file uploads. `url` is the CDN's and `address` is the load
 * balancer's, which is why the contract has both.
 *
 * The idle timeout is raised to an hour to match. AWS's own guidance is that a
 * target's keep-alive must be at least the balancer's idle timeout, and
 * `apps/api` sets its Node `keepAliveTimeout` to agree; the pair is what stops
 * a long-idle exec attach from being closed with a 408.
 *
 * Capabilities are expanded here, and this is the only file that knows what
 * *read telemetry* or *vend a volume credential* means on AWS. Each becomes an
 * inline policy on the task role, and each action list is one the API
 * demonstrably calls — see `storage.ts` for why that matters more than it
 * usually does.
 */

import type { Api, ApiCapability, ApiDependencies, ApiProvider, ApiRequest } from '../../api.ts'
import { CACHE_PASSWORD_VARIABLE } from '../../cache.ts'
import { CLICKHOUSE_PASSWORD_VARIABLE } from '../../clickhouse.ts'
import type { WorkloadHost } from '../../cluster.ts'
import { DATABASE_PASSWORD_VARIABLE } from '../../database.ts'
import type { Placement } from '../../network.ts'
import type { StorageBinding } from '../../storage.ts'

const HEALTH = {
  interval: '30 seconds',
  timeout: '5 seconds',
  healthyThreshold: 2,
  unhealthyThreshold: 3,
} as const

/** An hour, to match `apps/api`'s own keep-alive. See the note above. */
const IDLE_TIMEOUT_SECONDS = 3_600

const onAws = (storage: { binding: StorageBinding }): Extract<StorageBinding, { cloud: 'aws' }> => {
  if (storage.binding.cloud !== 'aws') throw new Error(`The AWS API was handed ${storage.binding.cloud} storage`)
  return storage.binding
}

/**
 * One capability, as the policy documents it becomes.
 *
 * A closed switch rather than a lookup table: a capability added to the
 * contract and not to this switch is a compile error here, which is the only
 * place it could be caught before the request that needed it.
 */
const grantsFor = (capability: ApiCapability): $util.Input<string>[] => {
  switch (capability.kind) {
    case 'list-own-bucket':
      return [onAws(capability.storage).listGrant]
    case 'manage-volume-buckets':
      return [onAws(capability.storage).lifecycleGrant]
    case 'vend-volume-credentials':
      return [
        onAws(capability.storage).credentialVending.roleArn.apply((arn: string) =>
          JSON.stringify({
            Version: '2012-10-17',
            Statement: [{ Effect: 'Allow', Action: ['sts:AssumeRole'], Resource: [arn] }],
          }),
        ),
      ]
    case 'read-telemetry':
      // The password is delivered by reference, so the task role has to be
      // allowed to resolve that reference. Nothing else in Secrets Manager.
      return capability.clickhouse.active
        ? [
            capability.clickhouse.reader.passwordRef.apply((arn: string) =>
              JSON.stringify({
                Version: '2012-10-17',
                Statement: [{ Effect: 'Allow', Action: ['secretsmanager:GetSecretValue'], Resource: [arn] }],
              }),
            ),
          ]
        : []
    case 'read-secret':
      return [
        capability.ref.apply((arn: string) =>
          JSON.stringify({
            Version: '2012-10-17',
            Statement: [{ Effect: 'Allow', Action: ['secretsmanager:GetSecretValue'], Resource: [arn] }],
          }),
        ),
      ]
  }
}

export const awsApiProvider =
  ({
    dependencies,
    dns,
    domain,
  }: {
    dependencies: ApiDependencies
    dns: ReturnType<typeof sst.cloudflare.dns>
    /** The root domain the CDN answers on. */
    domain: string
  }): ApiProvider =>
  (request: ApiRequest): Api => {
    const host = dependencies.host as Extract<WorkloadHost, { cloud: 'aws' }>
    const placement = dependencies.placement as Extract<Placement, { cloud: 'aws' }>
    const storage = onAws(dependencies.storage)

    const service = new sst.aws.Service(
      'Api',
      {
        cluster: host.cluster,
        wait: true,
        image: request.image,
        loadBalancer: {
          domain: { name: `api.${domain}`, dns },
          rules: [{ listen: '443/https', forward: `${request.port}/http` }],
          // The API is mounted globally under /api, so the balancer's default
          // probe of `/` would mark every healthy task unhealthy.
          health: { [`${request.port}/http`]: { path: '/api/health', ...HEALTH } },
        },
        environment: request.environment,
        // Every name that reaches the container by reference: the store's own
        // addresses, and the three passwords the stack decided.
        ssm: {
          ...request.secrets,
          [DATABASE_PASSWORD_VARIABLE]: dependencies.database.binding.passwordRef,
          [CACHE_PASSWORD_VARIABLE]: dependencies.cache.binding.passwordRef,
          ...(dependencies.clickhouse.active
            ? { [CLICKHOUSE_PASSWORD_VARIABLE]: dependencies.clickhouse.reader.passwordRef }
            : {}),
        },
        transform: {
          loadBalancer: (args: any) => {
            args.loadBalancerType = 'application'
            args.idleTimeout = IDLE_TIMEOUT_SECONDS
          },
          // The reachability the network arranged, plus membership of the two
          // data stores' own groups. Attached rather than assumed: a task
          // without them cannot reach the database however correct its
          // credentials are.
          service: (args: any) => {
            args.networkConfiguration = {
              ...args.networkConfiguration,
              subnets: placement.subnets,
              securityGroups: [
                ...placement.securityGroups,
                dependencies.database.binding.clientGrant,
                dependencies.cache.binding.clientGrant,
              ],
              assignPublicIp: false,
            }
          },
        },
      },
      { dependsOn: dependencies.waitFor },
    )

    /*
     * The role the API assumes to vend a per-organization credential.
     *
     * Created here rather than in the storage module because its trust policy
     * has to name the task role, which exists only once the service does. The
     * storage module declared its *name* up front, which is what breaks the
     * cycle: the API's environment could carry the name before the role was
     * real.
     */
    const vending = new aws.iam.Role('S3AccessRole', {
      name: storage.credentialVending.roleName,
      assumeRolePolicy: service.nodes.taskRole.arn.apply((taskRoleArn: string) =>
        JSON.stringify({
          Version: '2012-10-17',
          Statement: [{ Effect: 'Allow', Principal: { AWS: taskRoleArn }, Action: 'sts:AssumeRole' }],
        }),
      ),
    })
    // What a vended session may do at most. The per-organization session policy
    // the API attaches narrows it further; effective access is the intersection.
    new aws.iam.RolePolicy('S3AccessRolePolicy', {
      role: vending.name,
      policy: storage.bucketArn.apply((bucketArn: string) =>
        JSON.stringify({
          Version: '2012-10-17',
          Statement: [
            { Effect: 'Allow', Action: ['s3:GetObject', 's3:PutObject'], Resource: [`${bucketArn}/*`] },
            { Effect: 'Allow', Action: ['s3:ListBucket'], Resource: [bucketArn] },
          ],
        }),
      ),
    })

    const attached = request.capabilities.flatMap(grantsFor).map(
      (policy, index) =>
        new aws.iam.RolePolicy(`ApiCapability${index}`, {
          role: service.nodes.taskRole.name,
          policy,
        }),
    )

    /*
     * The CDN, which serves the dashboard's static assets at the root domain.
     *
     * SST's Router creates its placeholder origin as `http-only`, and that wins
     * over the per-request override its CloudFront function sets for HTTPS
     * origins — CloudFront then refuses the TLS handshake and answers 502.
     * Flipping it to `https-only` is what makes the function's override take.
     */
    const cdn = new sst.aws.Router('ApiCdn', {
      domain: { name: domain, dns },
      transform: {
        cdn: (args: any) => {
          args.origins = $util.output(args.origins).apply((origins: any[]) =>
            (origins ?? []).map((origin: any) => ({
              ...origin,
              customOriginConfig: origin.customOriginConfig
                ? { ...origin.customOriginConfig, originProtocolPolicy: 'https-only', originReadTimeout: 60 }
                : origin.customOriginConfig,
            })),
          )
        },
      },
    })
    cdn.route('/', service.url)

    return {
      url: cdn.url.apply((url: string) => url.replace(/\/$/, '')),
      address: service.url.apply((url: string) => url.replace(/\/$/, '')),
      identity: service.nodes.taskRole.arn,
      // CloudWatch dimensions a load balancer by the tail of its ARN, from
      // `app/` onward. Taken from the resource rather than parsed out of a URL,
      // which is where it would quietly stop matching.
      metricTarget: service.nodes.loadBalancer.arnSuffix,
      ready: [service, ...attached],
    }
  }
