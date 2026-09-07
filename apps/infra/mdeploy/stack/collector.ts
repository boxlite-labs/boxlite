/*
 * The OpenTelemetry collector every other workload ships to.
 *
 * One address, reached from inside the network and from nowhere else. That is
 * the whole of the portable contract: the API, the proxy, the runner and the
 * boxes running on it all emit OTLP to `otlpUrl`, and none of them knows
 * whether an internal load balancer or a Cloud Run service with internal
 * ingress is what answered.
 *
 * `exporters` is the one setting that leaks a fact about ClickHouse into this
 * module, and it does so deliberately: the collector's pipelines are
 * reconfigured on the command line, so which exporters run has to be decided
 * before the container starts rather than discovered by it. A stage with no
 * ClickHouse runs the BoxLite exporter alone, and a collector told to write to
 * a database that does not exist would retry every batch forever.
 */

export type CollectorRequest = {
  image: $util.Input<string>
  /**
   * The exporter list each pipeline is set to, as the collector's own config
   * syntax spells it. Composed by the caller from what actually exists.
   */
  exporters: string
  /** Values the container reads, already assembled. */
  environment: Record<string, $util.Input<string>>
  /** Names it reads by reference: the ClickHouse writer password, and its own key. */
  secrets: Record<string, $util.Input<string>>
}

export type Collector = {
  /**
   * Where OTLP/HTTP is accepted, with no trailing slash.
   *
   * Every consumer concatenates a path onto it, and one that had to strip a
   * slash first would be four call sites each remembering to.
   */
  otlpUrl: $util.Output<string>
  ready: any[]
}

export type CollectorProvider = (request: CollectorRequest) => Collector

/** The ports the collector listens on. The same on both clouds. */
export const OTLP_HTTP_PORT = 4318
export const COLLECTOR_HEALTH_PORT = 13133
