/*
 * Two ways an image reaches a stage's registry: built there, or promoted there.
 *
 * `publish` builds every artifact at one commit and uploads it. The order is
 * deliberate and each step exists because of a specific failure:
 *
 *   ensure repository   a first publish into a fresh account would otherwise
 *                       fail on push, after paying for the build
 *   already published?  tags are immutable, so re-pushing a commit fails with
 *                       "cannot be overwritten". Re-running a green build is
 *                       not a failure, so it is checked first — and checked
 *                       *before* building, because building to then discover
 *                       the answer is wasted work
 *   build, push         the configured context; the commit is passed in as
 *                       REVISION so the image can name itself
 *   scan gate           a published image that nobody may deploy is better
 *                       than a deployed image nobody scanned. ECR registers
 *                       the scan after the push returns, so this waits for an
 *                       answer rather than reading whatever is there a second
 *                       later, which is either no scan or an unfinished one
 *
 * `promote` moves an already-built commit from one stage's registry to
 * another's. It never builds: the bytes that ran in dev are the bytes that run
 * in prod, and rebuilding from the same commit would only reintroduce the
 * chance that they differ. Layers are not shared between ECR repositories, so
 * this is a pull, a re-tag and a push rather than a manifest copy.
 *
 * `verifyPublished` asks the question both of them ask first — does this
 * registry hold every artifact at this commit — and does nothing with the
 * answer but report it. A deploy asks it before it applies anything: the
 * addresses it resolves are the ones a runtime pulls minutes later, and a
 * commit still being built surfaces there as a container that cannot start,
 * after an apply has already created resources.
 *
 * Every external call goes through `run`, injected, so the whole sequence is
 * testable without a registry, a daemon or credentials.
 */

import { join } from 'node:path'
import { setTimeout as sleep } from 'node:timers/promises'
import type { BuildConfig, ScanSeverity } from './config.ts'
import { addressFor, assertTag, type Registry } from './address.ts'

export class PublishError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'PublishError'
  }
}

/**
 * The scan gate's refusal, told apart from every other failure.
 *
 * A separate class because a caller retries a publish and must not retry this
 * one. Everything else `PublishError` reports is a push, a token endpoint or a
 * registry API — all of which fail transiently, which is why the retry exists.
 * A scan finding is an answer about the image's own contents, and asking again
 * returns it again: the calling workflow spent three attempts and ninety
 * seconds of backoff re-reading the same four findings before reporting them.
 *
 * The distinction is carried out to the process's exit code
 * (`bin/mbuild.ts`), because the caller that has to stop retrying is a shell.
 */
export class ScanRefusedError extends PublishError {
  constructor(message: string) {
    super(message)
    this.name = 'ScanRefusedError'
  }
}

/** One external command. Non-zero exit is reported, never thrown away. */
export type RunResult = { code: number; stdout: string; stderr: string }
/**
 * `stdin` is how a secret reaches a command without passing through argv.
 *
 * `echo` sends a command's output on to the log as it arrives, and is set for
 * the commands that take minutes — a build that prints nothing until it
 * finishes cannot be told from a build that has hung. It stays off for the
 * rest, because their output is a registry password or a document to parse,
 * and neither belongs in a log.
 */
export type RunOptions = { stdin?: string; echo?: boolean }
export type Run = (command: string, args: string[], options?: RunOptions) => Promise<RunResult>

/**
 * Time, injected for the same reason `run` is: a test spends a scan's whole
 * budget without spending the time. Both halves together, because a deadline
 * that reads the real clock and sleeps a fake one measures nothing.
 */
export type Clock = { now: () => number; wait: (milliseconds: number) => Promise<unknown> }
const systemClock: Clock = { now: Date.now, wait: sleep }

/** How often a scan that has not answered yet is asked again; `aws ecr wait` polls at this cadence. */
const SCAN_POLL_SECONDS = 5

export type PublishOutcome = {
  artifact: string
  address: string
  /** False when the commit was already there and nothing was built or moved. */
  built: boolean
}

/**
 * What a registry says about one image's scan right now.
 *
 * `pending` is the state that has to exist. A scan is registered after the push
 * that triggers it returns, so the first read of a just-pushed image finds no
 * scan at all, and the read after that finds one still running. Neither is an
 * answer, and neither is a failure — which is what separates them from
 * `failed`, an image this registry will never produce counts for.
 */
type ScanReport =
  | { state: 'pending'; detail: string }
  | { state: 'complete'; counts: Record<string, number> }
  | { state: 'failed'; detail: string }

type Registrar = {
  ensureRepository: () => Promise<void>
  isPublished: (artifact: string, tag: string) => Promise<boolean>
  login: () => Promise<void>
  scanReport: (artifact: string, tag: string) => Promise<ScanReport>
}

const ecrRegistrar = ({
  run,
  config,
  stage,
  registry,
}: {
  run: Run
  config: BuildConfig
  stage: string
  registry: Extract<Registry, { kind: 'ecr' }>
}): Registrar => {
  const declared = config.stages[stage]!.registry
  const aws = async (args: string[]): Promise<RunResult> => run('aws', [...args, '--region', registry.region])
  return {
    async ensureRepository() {
      const existing = await aws(['ecr', 'describe-repositories', '--repository-names', registry.repository])
      if (existing.code === 0) return
      // Immutable tags are what make a published commit mean exact bytes; scan
      // on push is what gives the gate below something to read.
      const created = await aws([
        'ecr',
        'create-repository',
        '--repository-name',
        registry.repository,
        '--image-tag-mutability',
        declared.immutableTags ? 'IMMUTABLE' : 'MUTABLE',
        '--image-scanning-configuration',
        `scanOnPush=${declared.scanOnPush}`,
      ])
      if (created.code !== 0) {
        throw new PublishError(`Could not create ${registry.repository}: ${created.stderr.trim()}`)
      }
    },
    async isPublished(artifact, tag) {
      const found = await aws([
        'ecr',
        'describe-images',
        '--repository-name',
        registry.repository,
        '--image-ids',
        `imageTag=${tag}-${artifact}`,
      ])
      return found.code === 0
    },
    async login() {
      const password = await aws(['ecr', 'get-login-password'])
      if (password.code !== 0) throw new PublishError(`Could not obtain a registry password: ${password.stderr.trim()}`)
      // Fed through stdin, never argv, which the process table exposes. An
      // unfed `--password-stdin` is worse than insecure: docker prompts
      // instead, and fails on the absent terminal rather than on the password.
      const login = await run('docker', ['login', '--username', 'AWS', '--password-stdin', registry.host], {
        stdin: password.stdout.trim(),
      })
      if (login.code !== 0) throw new PublishError(`Could not log in to ${registry.host}: ${login.stderr.trim()}`)
    },
    async scanReport(artifact, tag) {
      const answer = await aws([
        'ecr',
        'describe-image-scan-findings',
        '--repository-name',
        registry.repository,
        '--image-id',
        `imageTag=${tag}-${artifact}`,
        // The status travels with the counts because neither can be read alone:
        // absent counts mean "found nothing" once the scan is COMPLETE and
        // "has not looked yet" until then, and those are opposite answers.
        '--query',
        '{status: imageScanStatus.status, detail: imageScanStatus.description, counts: imageScanFindings.findingSeverityCounts}',
        // JSON rather than text, for the state this is waiting through: a
        // response for a scan still running carries no `imageScanFindings`, and
        // the CLI's text formatter writes this paginated operation's result keys
        // into the null it stored for that field and dies on the assignment.
        '--output',
        'json',
      ])
      if (answer.code !== 0) {
        // The image exists before its scan does: the push registers the scan on
        // its way out, and this read can arrive first.
        if (answer.stderr.includes('ScanNotFoundException')) return { state: 'pending', detail: 'not registered yet' }
        throw new PublishError(`Could not read scan findings: ${answer.stderr.trim()}`)
      }
      let described: { status?: string; detail?: string; counts?: Record<string, number> | null }
      try {
        described = (JSON.parse(answer.stdout || '{}') ?? {}) as typeof described
      } catch {
        throw new PublishError('Scan findings were not valid JSON')
      }
      const status = described.status ?? 'UNKNOWN'
      switch (status) {
        // COMPLETE is what a scan on push reports; ACTIVE is what continuous
        // scanning reports for an image whose findings are current. Enhanced
        // scanning is a registry-wide setting, so the same repository can start
        // answering the second without this config changing.
        case 'COMPLETE':
        case 'ACTIVE':
          return { state: 'complete', counts: described.counts ?? {} }
        case 'IN_PROGRESS':
        case 'PENDING':
          return { state: 'pending', detail: status }
        default:
          // Everything else is terminal — an unsupported image is the one that
          // happens — and waiting out the budget would only replace the reason
          // ECR gave with a timeout.
          return { state: 'failed', detail: described.detail ? `${status}: ${described.detail}` : status }
      }
    },
  }
}

/**
 * Artifact Registry, which answers the same four questions differently.
 *
 * Two differences are worth naming. A repository holds one artifact per name
 * rather than one repository holding every artifact under a compound tag, so
 * the address puts the artifact in the path and the tag is just the commit —
 * `address.ts` already writes both shapes.
 *
 * And scanning is not part of a push. Artifact Analysis scans continuously and
 * answers per occurrence, so the gate reads occurrences rather than waiting for
 * a scan to attach to the image, and never reports one as pending. When
 * analysis is not enabled the query returns nothing, which reads as no
 * findings, so `scanOnPush` in the config is what keeps this gate honest. ECR
 * answers a missing scan differently: it reads as pending there, and the
 * publish fails on a spent budget rather than on an empty answer.
 */
const artifactRegistryRegistrar = ({
  run,
  registry,
}: {
  run: Run
  registry: Extract<Registry, { kind: 'artifact-registry' }>
}): Registrar => {
  const region = registry.host.replace(/-docker\.pkg\.dev$/, '')
  const gcloud = async (args: string[]): Promise<RunResult> =>
    run('gcloud', [...args, '--project', registry.project, '--quiet'])
  const path = `${registry.host}/${registry.project}/${registry.repository}`
  return {
    async ensureRepository() {
      const existing = await gcloud([
        'artifacts',
        'repositories',
        'describe',
        registry.repository,
        '--location',
        region,
      ])
      if (existing.code === 0) return
      const created = await gcloud([
        'artifacts',
        'repositories',
        'create',
        registry.repository,
        '--location',
        region,
        '--repository-format',
        'docker',
      ])
      if (created.code !== 0) {
        throw new PublishError(`Could not create ${registry.repository}: ${created.stderr.trim()}`)
      }
    },
    async isPublished(artifact, tag) {
      const found = await gcloud(['artifacts', 'docker', 'images', 'describe', `${path}/${artifact}:${tag}`])
      return found.code === 0
    },
    async login() {
      // No password to fetch: the Docker credential helper reads the ambient
      // Application Default Credentials every time, so there is nothing here
      // that expires between this call and the push.
      const configured = await run('gcloud', ['auth', 'configure-docker', registry.host, '--quiet'])
      if (configured.code !== 0) {
        throw new PublishError(`Could not log in to ${registry.host}: ${configured.stderr.trim()}`)
      }
    },
    async scanReport(artifact, tag) {
      const answer = await gcloud([
        'artifacts',
        'docker',
        'images',
        'describe',
        `${path}/${artifact}:${tag}`,
        '--show-package-vulnerability',
        '--format',
        'json',
      ])
      if (answer.code !== 0) throw new PublishError(`Could not read scan findings: ${answer.stderr.trim()}`)
      let described: { package_vulnerability_summary?: { vulnerabilities?: Record<string, unknown[]> } }
      try {
        described = JSON.parse(answer.stdout || '{}') ?? {}
      } catch {
        throw new PublishError('Scan findings were not valid JSON')
      }
      // Artifact Analysis reports one entry per occurrence; the gate counts by
      // severity, so they are counted here rather than everywhere they are read.
      const vulnerabilities = described.package_vulnerability_summary?.vulnerabilities ?? {}
      // Never pending: this reads the occurrences that exist now rather than a
      // scan attached to the push, so there is nothing here to wait for.
      return {
        state: 'complete',
        counts: Object.fromEntries(
          Object.entries(vulnerabilities).map(([severity, occurrences]) => [severity, occurrences.length]),
        ),
      }
    },
  }
}

const registrarFor = ({
  run,
  config,
  stage,
  registry,
}: {
  run: Run
  config: BuildConfig
  stage: string
  registry: Registry
}): Registrar => {
  switch (registry.kind) {
    case 'ecr':
      return ecrRegistrar({ run, config, stage, registry })
    case 'artifact-registry':
      return artifactRegistryRegistrar({ run, registry })
  }
}

const blockingFindings = (counts: Record<string, number>, blockOn: ScanSeverity[]): string[] =>
  blockOn.filter((severity) => (counts[severity] ?? 0) > 0).map((severity) => `${counts[severity]} ${severity}`)

/**
 * The artifacts a registry does not hold at one commit, in declared order.
 *
 * The names rather than a yes or no: both callers report which artifact is
 * missing, and a boolean would leave each of them to find out again.
 */
const missingFrom = async ({
  registrar,
  artifacts,
  tag,
}: {
  registrar: Registrar
  artifacts: string[]
  tag: string
}): Promise<string[]> => {
  const missing: string[] = []
  for (const artifact of artifacts) {
    if (!(await registrar.isPublished(artifact, tag))) missing.push(artifact)
  }
  return missing
}

/**
 * One image's severity counts, waited for.
 *
 * A registry that has not answered yet is not an image with nothing found, and
 * reading it as one lets an unscanned image through the gate that exists to
 * scan it. Reading it as a failure is the opposite mistake: it fails a publish
 * whose image is fine, seconds before the answer would have arrived. So a
 * pending report is asked again, and `scan.timeoutSeconds` is how long that is
 * worth doing before no answer is itself the answer.
 */
const awaitScanCounts = async ({
  registrar,
  artifact,
  address,
  tag,
  timeoutSeconds,
  clock,
  log,
}: {
  registrar: Registrar
  artifact: string
  address: string
  tag: string
  timeoutSeconds: number
  clock: Clock
  log: (line: string) => void
}): Promise<Record<string, number>> => {
  // A deadline rather than a count of polls: the reads themselves take time, so
  // a budget spent in fixed steps is not the budget the config declared, and
  // the failure would name a duration that never elapsed. The last wait is
  // whatever is left of it, so a budget shorter than one interval still buys a
  // second read rather than none.
  const deadline = clock.now() + timeoutSeconds * 1_000
  let announced = false
  for (;;) {
    const report = await registrar.scanReport(artifact, tag)
    if (report.state === 'complete') return report.counts
    if (report.state === 'failed') throw new PublishError(`${address} was not scanned: ${report.detail}`)
    const remaining = deadline - clock.now()
    if (remaining <= 0) throw new PublishError(`${address} had no scan result after ${timeoutSeconds}s`)
    // Once, not per poll: a minute of the same sentence is no more informative
    // than one line of it, and a step that says nothing at all for as long as
    // this waits cannot be told from a hung one.
    if (!announced) {
      log(`Waiting up to ${timeoutSeconds}s for the scan of ${address} (${report.detail})`)
      announced = true
    }
    await clock.wait(Math.min(SCAN_POLL_SECONDS * 1_000, remaining))
  }
}

const assertNoBlockingFindings = async ({
  registrar,
  config,
  outcomes,
  tag,
  clock,
  log,
}: {
  registrar: Registrar
  config: BuildConfig
  outcomes: PublishOutcome[]
  tag: string
  clock: Clock
  log: (line: string) => void
}): Promise<void> => {
  for (const { artifact, address } of outcomes) {
    const counts = await awaitScanCounts({
      registrar,
      artifact,
      address,
      tag,
      timeoutSeconds: config.scan.timeoutSeconds,
      clock,
      log,
    })
    const blocking = blockingFindings(counts, config.scan.blockOn)
    if (blocking.length > 0) throw new ScanRefusedError(`${address} has ${blocking.join(' and ')} findings`)
  }
}

export const publish = async ({
  config,
  stage,
  registry,
  tag,
  run,
  log = console.log,
  clock = systemClock,
}: {
  config: BuildConfig
  stage: string
  registry: Registry
  tag: string
  run: Run
  log?: (line: string) => void
  clock?: Clock
}): Promise<PublishOutcome[]> => {
  assertTag(tag)
  const registrar = registrarFor({ run, config, stage, registry })
  await registrar.ensureRepository()
  await registrar.login()

  const outcomes: PublishOutcome[] = []
  for (const [artifact, declared] of Object.entries(config.artifacts)) {
    // Resolved against the repository, so the same command builds the same
    // bytes from apps/infra, from the root, or from a workflow step.
    const dockerfile = join(config.repository, declared.dockerfile)
    const context = join(config.repository, declared.context)
    const address = addressFor({ config, registry, artifact, tag })
    if (await registrar.isPublished(artifact, tag)) {
      log(`${address} is already published; nothing to do.`)
      outcomes.push({ artifact, address, built: false })
      continue
    }
    // Named before it starts, and echoed while it runs. The two long commands
    // in a publish are these, so a step that says nothing here says nothing at
    // all for as long as the build takes.
    log(`Building ${artifact} from ${dockerfile} as ${address}`)
    const built = await run(
      'docker',
      ['build', '--build-arg', `REVISION=${tag}`, '-f', dockerfile, '-t', address, context],
      { echo: true },
    )
    // The exit code, not the captured output: docker has already written the
    // reason to the log above, and repeating it here would print it twice.
    if (built.code !== 0) throw new PublishError(`Could not build ${artifact}: docker build exited ${built.code}`)
    log(`Pushing ${address}`)
    const pushed = await run('docker', ['push', address], { echo: true })
    if (pushed.code !== 0) throw new PublishError(`Could not push ${address}: docker push exited ${pushed.code}`)
    log(`Pushed ${address}`)
    outcomes.push({ artifact, address, built: true })
  }

  await assertNoBlockingFindings({ registrar, config, outcomes, tag, clock, log })
  return outcomes
}

/** Where one artifact sits, for a commit the registry already holds. */
export type PublishedImage = { artifact: string; address: string }

/**
 * Refuse unless a stage's registry already holds every artifact at one commit,
 * and report where they sit.
 *
 * A read: nothing is created, nothing is logged in to, nothing is built. That
 * is what makes it cheap enough to run before a deploy has committed to
 * anything, which is the point — the alternative is an apply that creates half
 * a stack and then fails on a task that cannot pull.
 *
 * The addresses come from `addressFor`, the same function the deploy resolves
 * its images through. A caller that composed its own string could check one
 * address and deploy another.
 */
export const verifyPublished = async ({
  config,
  stage,
  registry,
  tag,
  run,
}: {
  config: BuildConfig
  stage: string
  registry: Registry
  tag: string
  run: Run
}): Promise<PublishedImage[]> => {
  assertTag(tag)
  const registrar = registrarFor({ run, config, stage, registry })
  const artifacts = Object.keys(config.artifacts)
  const missing = await missingFrom({ registrar, artifacts, tag })
  if (missing.length > 0) {
    // The address, not the artifact name: it names the repository and the tag
    // that were looked for, which is what a person compares against the
    // publish that was supposed to have written them.
    const addresses = missing.map((artifact) => addressFor({ config, registry, artifact, tag }))
    throw new PublishError(`${stage} does not hold ${addresses.join(', ')}`)
  }
  return artifacts.map((artifact) => ({ artifact, address: addressFor({ config, registry, artifact, tag }) }))
}

export type PromoteOutcome = PublishOutcome & { from: string }

/**
 * Move a commit from one stage's registry to another's.
 *
 * The source must already hold every artifact: promoting half a release would
 * leave the destination with a version that cannot start. That is checked
 * before anything is pulled, so a missing artifact fails without having moved
 * the others.
 */
export const promote = async ({
  config,
  tag,
  from,
  to,
  run,
  log = console.log,
  clock = systemClock,
}: {
  config: BuildConfig
  tag: string
  from: { stage: string; registry: Registry }
  to: { stage: string; registry: Registry }
  run: Run
  log?: (line: string) => void
  clock?: Clock
}): Promise<PromoteOutcome[]> => {
  assertTag(tag)
  if (from.stage === to.stage) throw new PublishError(`Promoting "${from.stage}" to itself would do nothing`)

  const source = registrarFor({ run, config, stage: from.stage, registry: from.registry })
  const destination = registrarFor({ run, config, stage: to.stage, registry: to.registry })

  const artifacts = Object.keys(config.artifacts)
  const missing = await missingFrom({ registrar: source, artifacts, tag })
  if (missing.length > 0) {
    throw new PublishError(`${from.stage} does not hold ${missing.join(', ')} at ${tag}; nothing was promoted`)
  }

  await destination.ensureRepository()
  await source.login()
  await destination.login()

  const outcomes: PromoteOutcome[] = []
  for (const artifact of artifacts) {
    const sourceAddress = addressFor({ config, registry: from.registry, artifact, tag })
    const address = addressFor({ config, registry: to.registry, artifact, tag })
    if (await destination.isPublished(artifact, tag)) {
      log(`${address} is already there; nothing to do.`)
      outcomes.push({ artifact, address, from: sourceAddress, built: false })
      continue
    }
    // Pull, re-tag, push. ECR does not share layers between repositories, so
    // there is no manifest-only copy that would work here. The two transfers
    // are echoed for the same reason a build is: they move whole images.
    log(`Pulling ${sourceAddress}`)
    const pulled = await run('docker', ['pull', sourceAddress], { echo: true })
    if (pulled.code !== 0) throw new PublishError(`Could not pull ${sourceAddress}: docker pull exited ${pulled.code}`)
    const tagged = await run('docker', ['tag', sourceAddress, address])
    if (tagged.code !== 0) throw new PublishError(`Could not tag ${address}: ${tagged.stderr.trim()}`)
    log(`Pushing ${address}`)
    const pushed = await run('docker', ['push', address], { echo: true })
    if (pushed.code !== 0) throw new PublishError(`Could not push ${address}: docker push exited ${pushed.code}`)
    log(`Promoted ${sourceAddress} to ${address}`)
    outcomes.push({ artifact, address, from: sourceAddress, built: true })
  }

  // The destination scans on push too, and its threshold is the one that
  // decides whether the receiving stage may run this.
  await assertNoBlockingFindings({ registrar: destination, config, outcomes, tag, clock, log })
  return outcomes
}
