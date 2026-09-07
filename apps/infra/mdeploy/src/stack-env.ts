/*
 * What one deploy reads out of the environment, in one place.
 *
 * mdeploy puts the stage's configuration into the environment before the stack
 * runs, and the stack turns some of it into each workload's own environment.
 * That turning is policy, not plumbing: which keys are required, which pair
 * with each other, which get composed into a single value the service's schema
 * accepts.
 *
 * Both engines call this — `sst.config.ts` on the AWS path and
 * `pulumi/program.ts` on the GCP one — and that is the whole point. Two copies
 * that drifted would deploy a control plane behaving differently on one cloud,
 * which is the kind of difference found months later, with a matching digest
 * and nothing reporting it. `test/stack-env.test.ts` asserts that only one copy
 * exists. The API's own environment is one layer further down, in
 * `src/api-environment.ts`, for the same reason.
 *
 * It takes an environment rather than reading `process.env`, because the Pulumi
 * program runs inside the driver's own process: reading the ambient environment
 * there would read whatever that shell happened to hold.
 */

import { apiEnvironmentFrom } from './api-environment.ts'
import { OTEL_GROUP, PROXY_GROUP, RUNNER_GROUP, serviceSecretsFrom, splitServiceChannels, type GroupDeclaration } from './env.ts'
import type { RunnerBinary, RunnerSlot } from '../stack/runners.ts'
import type { MstageConfig } from 'mstage/config'

export class StackEnvError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'StackEnvError'
  }
}

/** Configuration that must be supplied, with the reason it matters. */
const required = (environment: NodeJS.ProcessEnv, key: string, why: string): string => {
  const value = environment[key]?.trim()
  if (!value) throw new StackEnvError(`${key} is required — ${why}`)
  return value
}

const optional = (environment: NodeJS.ProcessEnv, key: string): string | null => environment[key]?.trim() || null

/** At most this many hosts in one fleet, which is `runner/model/inventory.ts`'s own bound. */
const MAX_RUNNERS = 100

/** What the control plane accepts as a runner's registered name. */
const RUNNER_NAME = /^[a-zA-Z0-9_.-]{2,255}$/

/**
 * The fleet, as slots rather than as a count.
 *
 * The first host keeps the resource name `Runner` and the operator's chosen
 * control-plane name; every later one is numbered. Both facts are load-bearing
 * and neither is cosmetic: the resource name is what a targeted deploy selects
 * on and what an existing stage's state already calls its first host, so
 * renaming it would replace a machine rather than update one.
 */
const fleetFrom = (environment: NodeJS.ProcessEnv): RunnerSlot[] => {
  const raw = optional(environment, 'RUNNERS') ?? '1'
  if (!/^[1-9][0-9]*$/.test(raw) || Number(raw) > MAX_RUNNERS) {
    throw new StackEnvError(`RUNNERS must be a whole number from 1 to ${MAX_RUNNERS}; got ${JSON.stringify(raw)}`)
  }
  const defaultName = optional(environment, 'DEFAULT_RUNNER_NAME') ?? 'default'
  if (!RUNNER_NAME.test(defaultName)) {
    throw new StackEnvError(
      'DEFAULT_RUNNER_NAME must be 2-255 characters of letters, digits, underscores, periods and hyphens',
    )
  }
  return Array.from({ length: Number(raw) }, (_, offset) => {
    const index = offset + 1
    return index === 1
      ? { resourceName: 'Runner', nameTag: 'boxlite-runner-default', controlPlaneRunnerName: defaultName }
      : {
          resourceName: `Runner-runner-${index}`,
          nameTag: `boxlite-runner-${index}`,
          controlPlaneRunnerName: `runner-${index}`,
        }
  })
}

/**
 * What each host installs.
 *
 * Both halves or neither is not enough here — a URL with no checksum is a host
 * that installs whatever answered, which is the one thing a checksum exists to
 * prevent. So both are required, and the failure names the pair rather than
 * whichever was read first.
 */
const binaryFrom = (environment: NodeJS.ProcessEnv): RunnerBinary => {
  const url = required(environment, 'BOXLITE_RUNNER_BINARY_URL', 'a host has to be told what to install')
  const sha256 = required(
    environment,
    'BOXLITE_RUNNER_BINARY_SHA256',
    'a host installs what the checksum proves, not what answered the URL',
  )
  if (!/^[0-9a-f]{64}$/.test(sha256)) {
    throw new StackEnvError('BOXLITE_RUNNER_BINARY_SHA256 must be one lowercase hex SHA-256 digest')
  }
  const source = optional(environment, 'BOXLITE_RUNNER_BINARY_SOURCE') ?? 'release'
  if (source !== 'release' && source !== 'build') {
    throw new StackEnvError(`BOXLITE_RUNNER_BINARY_SOURCE must be "release" or "build"; got ${JSON.stringify(source)}`)
  }
  return { url, sha256, source }
}

/**
 * A ClickHouse someone else operates, or nothing.
 *
 * All three or none. An endpoint with no credentials is a database this stage
 * cannot authenticate to, and credentials with no endpoint are a secret for
 * nothing — so a stage that supplied two of the three is a mistake worth naming
 * here rather than a `mode: managed` deploy that fails halfway.
 */
const managedClickHouseFrom = (
  environment: NodeJS.ProcessEnv,
): { url: string; writerSecretArn: string; readerSecretArn: string } | null => {
  const parts = {
    url: optional(environment, 'CLICKHOUSE_URL'),
    writerSecretArn: optional(environment, 'CLICKHOUSE_WRITER_PASSWORD_SECRET_ARN'),
    readerSecretArn: optional(environment, 'CLICKHOUSE_READER_PASSWORD_SECRET_ARN'),
  }
  const present = Object.entries(parts).filter(([, value]) => value !== null)
  if (present.length === 0) return null
  if (present.length !== 3) {
    throw new StackEnvError(
      'CLICKHOUSE_URL, CLICKHOUSE_WRITER_PASSWORD_SECRET_ARN and CLICKHOUSE_READER_PASSWORD_SECRET_ARN ' +
        `must be set together; this stage set only ${present.map(([key]) => key).join(', ')}`,
    )
  }
  return parts as { url: string; writerSecretArn: string; readerSecretArn: string }
}

/** Everything one deploy decides, as opposed to what `mdeploy.config.json` declares. */
export type StackEnvironment = {
  /** The commit being deployed. Every container runs the same one. */
  tag: string
  /** The hostname the dashboard and the SDKs reach the control plane on. */
  domain: string
  proxyDomain: string
  proxyProtocol: string
  /** The verified sender domain, or null for a stage that sends no mail. */
  senderDomain: string | null
  runnerFleet: RunnerSlot[]
  runnerBinary: RunnerBinary
  /** A ClickHouse someone else operates, for a stage that runs none of its own. */
  managedClickHouse: { url: string; writerSecretArn: string; readerSecretArn: string } | null
  /**
   * The DNS zone every public record is written into.
   *
   * Required on both clouds even though only one of them reads it here: SST's
   * Cloudflare adapter discovers the zone from the domain, and the Pulumi path
   * has no adapter to discover anything — so a GCP stage names it. Read for
   * both so a stage that moves clouds does not discover a missing key on the
   * far side of the move.
   */
  dnsZoneId: string
  /**
   * The SMTP relay a GCP stage sends through, or null.
   *
   * Google has no sending service, so a GCP stage that names a sender domain
   * has to name a relay as well. Null on AWS, where SES is the answer.
   */
  mailRelayHost: string | null
  apiEnvironment: Record<string, string>
  /** Addresses rather than values, for the keys `env.selectGroup.secret` marks. */
  apiSecrets: Record<string, string>
  proxyEnvironment: Record<string, string>
  proxySecrets: Record<string, string>
  collectorEnvironment: Record<string, string>
  collectorSecrets: Record<string, string>
  runnerEnvironment: Record<string, string>
  runnerSecrets: Record<string, string>
}

/**
 * Each workload's two channels: values, and addresses.
 *
 * Split rather than merged because the two travel differently — a value reaches
 * the container as plain environment, an address as an ECS `secrets` entry or a
 * Cloud Run `secretKeyRef` — and which cloud renders it is why `home` is here.
 */
const channels = ({
  environment,
  declaration,
  region,
  stage,
  home,
}: {
  environment: NodeJS.ProcessEnv
  declaration: GroupDeclaration
  region: string
  stage: string
  home: MstageConfig['home']
}) => {
  const api = apiEnvironmentFrom({ environment, declaration, region, stage, home })
  const split = (group: string) =>
    splitServiceChannels({
      delivered: serviceSecretsFrom({ group, declaration, environment }),
      declaration,
      home,
    })
  const proxy = split(PROXY_GROUP)
  const collector = split(OTEL_GROUP)
  const runner = split(RUNNER_GROUP)
  return {
    apiEnvironment: api.environment,
    apiSecrets: api.secrets,
    proxyEnvironment: proxy.values,
    proxySecrets: proxy.addresses,
    collectorEnvironment: collector.values,
    collectorSecrets: collector.addresses,
    runnerEnvironment: runner.values,
    runnerSecrets: runner.addresses,
  }
}

/**
 * One deploy's inputs, read and checked before anything is built.
 *
 * What is checked here is the shape of what arrived: every required key is
 * present, the tag is one full commit sha, the fleet is a number this
 * repository will create, and the runner binary came with the checksum that
 * proves it. The failure then names the key rather than the resource that
 * choked on it.
 */
export const readStackEnvironment = ({
  environment,
  declaration,
  stage,
  region,
  home,
}: {
  environment: NodeJS.ProcessEnv
  /** Where `env.selectGroup` is declared: which keys are each service's own. */
  declaration: GroupDeclaration
  stage: string
  region: string
  /** Which cloud's secret store an address in `env.selectGroup.secret` names. */
  home: MstageConfig['home']
}): StackEnvironment => {
  const tag = required(environment, 'BOXLITE_IMAGE_TAG', 'a deploy names the exact commit it ships')
  if (!/^[0-9a-f]{40}$/.test(tag)) throw new StackEnvError('BOXLITE_IMAGE_TAG must be one full lowercase commit SHA')
  return {
    tag,
    domain: required(environment, 'STACK_DOMAIN', 'the hostname the dashboard and the SDKs reach this stage on'),
    proxyDomain: required(environment, 'PROXY_DOMAIN', 'every box is a name under this zone'),
    // A default rather than a requirement: the proxy speaks to a box inside the
    // network, and `http` there is the shape every stage has used. What must
    // never be defaulted is the zone above, which is public.
    proxyProtocol: optional(environment, 'PROXY_PROTOCOL') ?? 'http',
    // Absent is a stage that sends no mail, which is a supported state — see
    // `stack/mail.ts`. An empty string is the same answer, spelled by a store
    // that holds the key with nothing in it.
    senderDomain: optional(environment, 'MAIL_DOMAIN'),
    runnerFleet: fleetFrom(environment),
    runnerBinary: binaryFrom(environment),
    managedClickHouse: managedClickHouseFrom(environment),
    dnsZoneId: required(environment, 'CLOUDFLARE_ZONE_ID', 'every public record this stage writes goes into it'),
    mailRelayHost: optional(environment, 'MAIL_RELAY_HOST'),
    ...channels({ environment, declaration, region, stage, home }),
  }
}
