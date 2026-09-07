/*
 * Alert policies on Cloud Monitoring.
 *
 * The difference from CloudWatch is not the policy, it is the metric. AWS reads
 * counters the workloads publish; Google reads *log-based* metrics, which do
 * not exist until something is told how to extract them. So each alarm here is
 * two resources — the metric that says which log lines count, and the policy
 * that says how many is too many — where the AWS side is one.
 *
 * That is why the shared module carries a threshold and nothing else. A
 * namespace, a dimension and a filter are three different clouds' words for the
 * same idea, and none of them is portable; a contract that named any of them
 * would have named one cloud's.
 *
 * `alignmentPeriod` and the comparison are chosen to match the AWS side's, so
 * an operator reading a page on one cloud is reading the same question on the
 * other: a count over a minute, over the threshold, for this many consecutive
 * periods.
 */

import type { AlarmProvider, AlarmRequest, AlarmSubjects, AlarmThreshold } from '../../alarms.ts'

/** One minute, matching the AWS side's period. */
const PERIOD = '60s'

/** One metric and the policy that watches it. */
const watch = ({
  resourceName,
  project,
  metricName,
  filter,
  threshold,
  description,
  notificationChannels,
}: {
  resourceName: string
  project: string
  metricName: string
  /** Which log entries count. Google's own logging filter syntax. */
  filter: $util.Input<string>
  threshold: AlarmThreshold
  description: string
  notificationChannels: string[]
}): any[] => {
  const metric = new gcp.logging.Metric(`${resourceName}Metric`, {
    name: metricName,
    project,
    filter,
    metricDescriptor: { metricKind: 'DELTA', valueType: 'INT64' },
  })
  const policy = new gcp.monitoring.AlertPolicy(resourceName, {
    project,
    displayName: description,
    combiner: 'OR',
    conditions: [
      {
        displayName: description,
        conditionThreshold: {
          filter: metric.name.apply(
            (name: string) => `metric.type="logging.googleapis.com/user/${name}" AND resource.type="cloud_run_revision"`,
          ),
          comparison: 'COMPARISON_GT',
          // One below the threshold with a strict comparison, so "at least N"
          // means the same thing here as CloudWatch's GreaterThanOrEqual.
          thresholdValue: threshold.threshold - 1,
          duration: `${threshold.periods * 60}s`,
          aggregations: [{ alignmentPeriod: PERIOD, perSeriesAligner: 'ALIGN_DELTA' }],
        },
      },
    ],
    notificationChannels,
  })
  return [metric, policy]
}

export const gcpAlarmProvider =
  ({
    subjects,
    project,
    notificationChannels = [],
  }: {
    subjects: AlarmSubjects
    project: string
    /** Where a firing policy is sent. Empty is a policy that only shows in the console. */
    notificationChannels?: string[]
  }): AlarmProvider =>
  (request: AlarmRequest): void => {
    const prefix = `${$app.name}-${$app.stage}`

    watch({
      resourceName: 'ApiServerErrorAlarm',
      project,
      metricName: `${prefix}-api-5xx`,
      filter: subjects.api.metricTarget.apply(
        (service: string) =>
          `resource.type="cloud_run_revision" AND resource.labels.service_name="${service}" AND httpRequest.status>=500`,
      ),
      threshold: request.apiServerErrors,
      description: 'The control plane is answering 5xx',
      notificationChannels,
    })

    watch({
      resourceName: 'ProxyUnhealthyTargetAlarm',
      project,
      metricName: `${prefix}-proxy-unhealthy`,
      // The group's own autohealer logs a repair when a host stops answering,
      // which is the closest thing this cloud has to AWS's UnHealthyHostCount.
      filter: subjects.edge.metricTarget.apply(
        (group: string) =>
          `resource.type="gce_instance_group_manager" AND resource.labels.instance_group_manager_name="${group}" ` +
          'AND jsonPayload.event_type="INSTANCE_REPAIR"',
      ),
      threshold: request.proxyUnhealthyTargets,
      description: 'The box proxy is repairing hosts; boxes may be unreachable',
      notificationChannels,
    })

    watch({
      resourceName: 'RunnerUnreachableAlarm',
      project,
      metricName: `${prefix}-runner-unreachable`,
      // Emitted by the control plane, not by the runner: a host that has gone
      // silent cannot report that it has.
      filter: subjects.api.metricTarget.apply(
        (service: string) =>
          `resource.type="cloud_run_revision" AND resource.labels.service_name="${service}" ` +
          'AND jsonPayload.event="runner.unreachable"',
      ),
      threshold: request.runnersUnreachable,
      description: 'A registered runner has stopped answering the control plane',
      notificationChannels,
    })
  }
