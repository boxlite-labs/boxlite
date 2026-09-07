/*
 * The machines boxes actually run on.
 *
 * Every other module in this directory describes something both clouds offer a
 * managed version of. This one does not, and that is the point of BoxLite: a
 * box is a microVM under KVM, so the host has to be a virtual machine that can
 * itself run a hypervisor. There is no serverless answer on either cloud, and a
 * contract that tried to hide the machine would have nothing left to describe.
 *
 * So `nestedVirtualization` is a request rather than an assumption, and both
 * providers have to satisfy it in their own way. AWS gives it on any modern
 * `.metal`-capable Nitro instance family without asking. GCP does not: nested
 * virtualization needs a machine family that supports it, a minimum CPU
 * platform of Haswell or later, and `advancedMachineFeatures.
 * enableNestedVirtualization` set explicitly — the exact three things
 * `scripts/deploy/gcp/create-instance.sh` has been doing by hand. A provider
 * that omitted any of them produces a host that boots, registers, and then
 * fails every box with no `/dev/kvm`.
 *
 * `size` is a request, never a machine type. `c8i.2xlarge` and `n2-standard-8`
 * are answers, and each cloud answers differently — including about which
 * families can nest at all.
 *
 * The binary is not built here. A runner installs a published release asset or
 * an object staged for one commit, and both reach it as a URL and a checksum;
 * which of the two a deploy chose is `RunnerBinary` below, resolved before any
 * machine exists so a bad selector fails before a host is created rather than
 * at its first boot.
 */

import type { Placement } from './network.ts'

export type RunnerSize = 'small' | 'medium' | 'large'

/** One host, as the control plane will know it. */
export type RunnerSlot = {
  /** The logical resource name. Stable, so a re-deploy updates rather than replaces. */
  resourceName: string
  /** What the machine is labelled with, for a person reading a console. */
  nameTag: string
  /** The name the control plane registers it under. Unique across the fleet. */
  controlPlaneRunnerName: string
}

/**
 * What a host installs, as a URL and the checksum that proves it.
 *
 * One shape for both sources — a published release and a per-commit object —
 * because the host does the same thing with either: fetch, verify, install.
 * Which one this is belongs in the deploy's log, not in the host's user data.
 */
export type RunnerBinary = {
  url: $util.Input<string>
  sha256: $util.Input<string>
  /** For the log line. A host never branches on it. */
  source: 'release' | 'build'
}

export type RunnerRequest = {
  size: RunnerSize
  rootDiskGb: number
  /**
   * Required, and named rather than assumed. See the note above: it is one flag
   * on AWS and three separate decisions on GCP.
   */
  nestedVirtualization: true
  fleet: readonly RunnerSlot[]
  binary: RunnerBinary
  /** Where a host registers itself, and where it ships telemetry. */
  apiUrl: $util.Input<string>
  otlpUrl: $util.Input<string>
  /** Values every host reads. */
  environment: Record<string, $util.Input<string>>
  /** Names it reads by reference: its own registration key, and the admin key. */
  secrets: Record<string, $util.Input<string>>
}

export type RunnerDependencies = {
  /**
   * The runner's own placement, taken straight from the network rather than
   * through a cluster: it is a machine, not a task, so there is no host to be
   * placed in — see `cluster.ts`. Its exposure is `egress-only-public`, and a
   * provider handed anything else should refuse rather than place a host that
   * cannot pull its own binary.
   */
  placement: Placement
  /** Nothing registers before the control plane can answer. */
  dependsOn: any[]
}

export type Runners = {
  /** One id per slot, in the order they were requested. */
  ids: $util.Output<string>[]
  ready: any[]
}

export type RunnerProvider = (request: RunnerRequest) => Runners

/** The one port a runner listens on: its API, the box proxy and the ssh gateway. */
export const RUNNER_PORT = 3003
