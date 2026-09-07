/*
 * What is watched once everything is serving.
 *
 * Alarms are last in the plan and nothing waits on them, which is the whole
 * reason they are a module of their own: one that cannot be created must not
 * roll back a service that is already answering requests. Declaring them beside
 * the workloads they watch would put them inside those workloads' rollbacks.
 *
 * A threshold is a count and a number of consecutive periods, on both clouds.
 * That much is genuinely shared. What is not shared is what a metric *is*: AWS
 * reads counters the API embeds in its own log lines, and Google reads
 * log-based metrics it has to be told how to extract first. Each provider owns
 * that translation, and this module never names a namespace or a filter.
 */

import type { Api } from './api.ts'
import type { Edge } from './edge.ts'
import type { Runners } from './runners.ts'

/** How bad, and for how long. Both clouds understand exactly this much. */
export type AlarmThreshold = {
  threshold: number
  /** Consecutive periods over the threshold before it fires. */
  periods: number
}

/**
 * The three things worth waking someone for, and no more.
 *
 * Deliberately short. An alarm nobody acts on trains people to close the page,
 * and each of these has a different action behind it: the API failing is a
 * rollback, the proxy losing its targets is a networking change, and a runner
 * going unreachable is capacity.
 */
export type AlarmRequest = {
  /** The control plane answering 5xx at all. */
  apiServerErrors: AlarmThreshold
  /** The box proxy's load balancer with no healthy target behind it. */
  proxyUnhealthyTargets: AlarmThreshold
  /** A registered runner the control plane has stopped hearing from. */
  runnersUnreachable: AlarmThreshold
}

/** What the alarms watch. Handles, so an alarm names the resource it is about. */
export type AlarmSubjects = {
  api: Api
  edge: Edge
  runners: Runners
}

export type AlarmProvider = (request: AlarmRequest) => void
