/*
 * CloudWatch alarms, on metrics the workloads already emit.
 *
 * Two of the three read a load balancer's own metrics, which exist whether or
 * not the application cooperates — that is the point of choosing them. The
 * third reads a counter the API embeds in its log lines, which is the only way
 * to ask a question about a runner from outside the runner.
 *
 * `treatMissingData: 'notBreaching'` on all three. A metric that stops
 * reporting is not the same as a metric over its threshold, and an alarm that
 * fires on silence fires every time a stage is quiet — which is how a page
 * stops being read.
 */

import type { AlarmProvider, AlarmRequest, AlarmSubjects, AlarmThreshold } from '../../alarms.ts'

/** One minute, which is the finest a standard metric is published at. */
const PERIOD_SECONDS = 60

/** The namespace the API's embedded metrics land in. */
const APPLICATION_NAMESPACE = 'BoxLite'

const alarm = (
  name: string,
  threshold: AlarmThreshold,
  spec: Record<string, unknown>,
): CloudResource =>
  new aws.cloudwatch.MetricAlarm(name, {
    comparisonOperator: 'GreaterThanOrEqualToThreshold',
    threshold: threshold.threshold,
    evaluationPeriods: threshold.periods,
    period: PERIOD_SECONDS,
    // Silence is not a breach. See the note above.
    treatMissingData: 'notBreaching',
    ...spec,
  })

export const awsAlarmProvider =
  ({ subjects }: { subjects: AlarmSubjects }): AlarmProvider =>
  (request: AlarmRequest): void => {
    alarm('ApiServerErrorAlarm', request.apiServerErrors, {
      namespace: 'AWS/ApplicationELB',
      metricName: 'HTTPCode_Target_5XX_Count',
      statistic: 'Sum',
      alarmDescription: 'The control plane is answering 5xx',
      // Dimensioned on the load balancer rather than on the target group: a
      // deploy replaces the group, and an alarm pointed at the old one goes
      // quiet exactly when a rollout is most likely to be the cause.
      dimensions: { LoadBalancer: subjects.api.metricTarget },
    })

    alarm('ProxyUnhealthyTargetAlarm', request.proxyUnhealthyTargets, {
      namespace: 'AWS/NetworkELB',
      metricName: 'UnHealthyHostCount',
      statistic: 'Maximum',
      alarmDescription: 'The box proxy has no healthy target; every running box is unreachable',
      dimensions: { LoadBalancer: subjects.edge.metricTarget },
    })

    alarm('RunnerUnreachableAlarm', request.runnersUnreachable, {
      namespace: APPLICATION_NAMESPACE,
      metricName: 'RunnerUnreachable',
      statistic: 'Maximum',
      alarmDescription: 'A registered runner has stopped answering the control plane',
    })
  }
