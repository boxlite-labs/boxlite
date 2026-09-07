/*
 * The collector, behind an internal load balancer.
 *
 * Internal is the whole security model here: OTLP carries no credential of its
 * own, so anything that can reach this endpoint can write telemetry attributed
 * to anything. `public: false` puts the balancer in the private subnets, where
 * only the workloads the network placed can reach it.
 *
 * Two listeners rather than one. OTLP/HTTP is what everything ships to; port 80
 * forwards to the collector's own health extension, which is what the load
 * balancer probes. They are separate because the OTLP port answers a bare `GET
 * /` with a 4xx — a health check pointed there would mark a healthy collector
 * unhealthy, which is why the OTLP listener's own probe accepts `200-499`.
 *
 * The ClickHouse password arrives as an ECS `secrets` entry rather than as an
 * environment value: the agent resolves it just before the container starts, so
 * it never enters the task definition.
 */

import type { ClickHouse } from '../../clickhouse.ts'
import { CLICKHOUSE_PASSWORD_VARIABLE } from '../../clickhouse.ts'
import type { Collector, CollectorProvider, CollectorRequest } from '../../collector.ts'
import { COLLECTOR_HEALTH_PORT, OTLP_HTTP_PORT } from '../../collector.ts'
import type { WorkloadHost } from '../../cluster.ts'

const HEALTH = {
  interval: '30 seconds',
  timeout: '5 seconds',
  healthyThreshold: 2,
  unhealthyThreshold: 3,
} as const

export const awsCollectorProvider =
  ({
    host,
    clickhouse,
    dependsOn,
  }: {
    host: Extract<WorkloadHost, { cloud: 'aws' }>
    clickhouse: ClickHouse
    dependsOn: any[]
  }): CollectorProvider =>
  (request: CollectorRequest): Collector => {
    const service = new sst.aws.Service(
      'OtelCollector',
      {
        cluster: host.cluster,
        image: request.image,
        // Wait for a healthy target only where there is a backend to be healthy
        // against: a stage with ClickHouse disabled has nothing for the
        // collector to fail to reach, and waiting would only lengthen the deploy.
        wait: clickhouse.active,
        command: [
          '--config',
          '/otelcol/collector-config.yaml',
          '--set',
          `service::pipelines::traces::exporters=${request.exporters}`,
          '--set',
          `service::pipelines::metrics::exporters=${request.exporters}`,
          '--set',
          `service::pipelines::logs::exporters=${request.exporters}`,
        ],
        loadBalancer: {
          public: false,
          rules: [
            { listen: `${OTLP_HTTP_PORT}/http`, forward: `${OTLP_HTTP_PORT}/http` },
            { listen: '80/http', forward: `${COLLECTOR_HEALTH_PORT}/http` },
          ],
          health: {
            // The OTLP port answers a bare GET with a 4xx, which is a running
            // collector rather than a broken one.
            [`${OTLP_HTTP_PORT}/http`]: { path: '/', ...HEALTH, successCodes: '200-499' },
            [`${COLLECTOR_HEALTH_PORT}/http`]: { path: '/health/status', ...HEALTH },
          },
        },
        environment: request.environment,
        ssm: {
          ...request.secrets,
          ...(clickhouse.active ? { [CLICKHOUSE_PASSWORD_VARIABLE]: clickhouse.writer.passwordRef } : {}),
        },
        transform: {
          loadBalancer: (args: any) => {
            args.loadBalancerType = 'application'
          },
        },
      },
      { dependsOn },
    )

    return {
      // Without a trailing slash and with the OTLP port, which is what every
      // consumer concatenates a path onto. Composed here so four call sites do
      // not each have to remember to strip one.
      otlpUrl: service.url.apply((url: string) => `${url.replace(/\/$/, '')}:${OTLP_HTTP_PORT}`),
      ready: [service],
    }
  }
