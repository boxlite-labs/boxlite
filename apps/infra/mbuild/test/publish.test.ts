import assert from 'node:assert/strict'
import { existsSync, readFileSync } from 'node:fs'
import test from 'node:test'
import { join } from 'node:path'
import { parseBuildConfig } from '../src/config.ts'
import { resolveRegistry } from '../src/address.ts'
import {
  promote,
  publish,
  PublishError,
  ScanRefusedError,
  verifyPublished,
  type Run,
  type RunResult,
} from '../src/publish.ts'

const config = parseBuildConfig(
  '/repo/apps/infra/mbuild.config.json',
  JSON.stringify({
    root: '../..',
    artifacts: {
      console: { dockerfile: 'apps/console/Dockerfile', context: '.' },
      api: { dockerfile: 'apps/api/Dockerfile', context: '.' },
    },
    scan: { blockOn: ['CRITICAL', 'HIGH'], timeoutSeconds: 300 },
    stages: {
      dev: {
        registry: {
          kind: 'ecr',
          repository: 'boxlite-app-dev',
          immutableTags: true,
          scanOnPush: true,
        },
      },
      prod: {
        registry: {
          kind: 'ecr',
          repository: 'boxlite-app-prod',
          immutableTags: true,
          scanOnPush: true,
        },
      },
    },
  }),
)

/** The same dev registry with one artifact, and a scan budget short enough for a test to spend. */
const withScanBudget = (timeoutSeconds: number) =>
  parseBuildConfig(
    '/repo/apps/infra/mbuild.config.json',
    JSON.stringify({
      root: '../..',
      artifacts: { api: { dockerfile: 'apps/api/Dockerfile', context: '.' } },
      scan: { blockOn: ['CRITICAL', 'HIGH'], timeoutSeconds },
      stages: {
        dev: {
          registry: {
            kind: 'ecr',
            repository: 'boxlite-app-dev',
            immutableTags: true,
            scanOnPush: true,
          },
        },
      },
    }),
  )

const briefBudget = withScanBudget(10)

/**
 * A clock a test drives. Waiting spends its own time rather than the real kind,
 * so a scan's whole budget goes by in an instant and every wait is recorded.
 */
const fakeClock = () => {
  const waits: number[] = []
  let elapsed = 0
  return {
    waits,
    clock: {
      now: () => elapsed,
      wait: async (milliseconds: number) => {
        waits.push(milliseconds)
        elapsed += milliseconds
      },
    },
  }
}

const SHA = 'b'.repeat(40)
const ACCOUNT = '000000000000'
// mstage declares these; mbuild's config does not repeat them.
const dev = resolveRegistry({ config, stage: 'dev', region: 'ap-southeast-1', accountId: ACCOUNT })
const prod = resolveRegistry({ config, stage: 'prod', region: 'us-east-1', accountId: ACCOUNT })

const ok = (stdout = ''): RunResult => ({ code: 0, stdout, stderr: '' })
const fail = (stderr: string): RunResult => ({ code: 1, stdout: '', stderr })

/**
 * Registries that answer like ECR, keyed by repository so one double can serve
 * both ends of a promotion. `published` holds `<repository>:<tag>-<artifact>`.
 *
 * `scan` is how a pushed image's scan behaves. `missingReads` reads answer
 * ScanNotFoundException before it exists, which is what ECR answers between a
 * push returning and its scan being registered, and `status` is what every read
 * after that reports. Only a finished scan carries counts.
 */
const registryDouble = ({
  published = new Set<string>(),
  repositories = new Set(['boxlite-app-dev', 'boxlite-app-prod']),
  findings = {},
  scan = {},
  pushFails,
}: {
  published?: Set<string>
  repositories?: Set<string>
  findings?: Record<string, number>
  scan?: { status?: string; description?: string; missingReads?: number }
  /** What a registry that is briefly unreachable looks like from `docker push`. */
  pushFails?: string
} = {}) => {
  const { status = 'COMPLETE', description, missingReads = 0 } = scan
  const unregistered = new Map<string, number>()
  const calls: string[][] = []
  const echoed: string[][] = []
  const logins: string[] = []
  const run: Run = async (command, args, options) => {
    calls.push([command, ...args])
    if (options?.echo) echoed.push([command, ...args])
    if (command === 'docker') {
      // What docker really does with an unfed `--password-stdin`: it falls back
      // to prompting, and a runner has no terminal to prompt on. A double that
      // accepts the login anyway cannot tell a piped password from a lost one.
      if (args[0] === 'login') {
        if (!options?.stdin) return fail('Error: Cannot perform an interactive login from a non TTY device')
        logins.push(options.stdin)
      }
      if (args[0] === 'push' && pushFails) return fail(pushFails)
      return ok()
    }
    const operation = `${args[0]} ${args[1]}`
    const named = (flag: string) => args[args.indexOf(flag) + 1]
    if (operation === 'ecr get-login-password') return ok('ecr-token\n')
    if (operation === 'ecr describe-repositories') {
      return repositories.has(named('--repository-names')!) ? ok() : fail('RepositoryNotFoundException')
    }
    if (operation === 'ecr describe-images') {
      const key = `${named('--repository-name')}:${named('--image-ids')!.replace('imageTag=', '')}`
      return published.has(key) ? ok() : fail('ImageNotFoundException')
    }
    if (operation === 'ecr describe-image-scan-findings') {
      const image = named('--image-id')!.replace('imageTag=', '')
      const reads = unregistered.get(image) ?? missingReads
      if (reads > 0) {
        unregistered.set(image, reads - 1)
        return fail(
          'An error occurred (ScanNotFoundException) when calling the DescribeImageScanFindings operation: ' +
            `Image scan does not exist for the image with '{imageTag:'${image}'}'`,
        )
      }
      // The projection the gate asks for: the status, and the counts that only
      // a finished scan has — COMPLETE from a scan on push, ACTIVE from the
      // continuous kind.
      const finished = status === 'COMPLETE' || status === 'ACTIVE'
      return ok(JSON.stringify({ status, detail: description, counts: finished ? findings : null }))
    }
    return ok()
  }
  return { calls, echoed, logins, run, ran: (predicate: (call: string[]) => boolean) => calls.filter(predicate) }
}

const lines: string[] = []
const log = (line: string) => lines.push(line)
const both = (repository: string) => new Set([`${repository}:${SHA}-console`, `${repository}:${SHA}-api`])

test('the registry password reaches docker through stdin, not through argv', async () => {
  // An unfed `--password-stdin` does not fail on the password: docker prompts
  // instead, and dies on the runner's missing terminal. That reads as a broken
  // workflow rather than as the dropped pipe it is.
  const probe = registryDouble()
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  assert.deepEqual(probe.logins, ['ecr-token'], 'the token aws printed is the token docker was fed')
  const login = probe.ran((call) => call[0] === 'docker' && call[1] === 'login')[0]!
  assert.ok(!login.includes('ecr-token'), 'a password in an argument list is readable from the process table')
})

test('a commit already published is not rebuilt, and is not an error', async () => {
  // Immutable tags make re-pushing fail, so a re-run of a green build has to be
  // recognised rather than attempted.
  const probe = registryDouble({ published: both('boxlite-app-dev') })
  const outcomes = await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  assert.deepEqual(
    outcomes.map((outcome) => outcome.built),
    [false, false],
  )
  assert.equal(probe.ran((call) => call[1] === 'build').length, 0)
})

test('only the missing artifact is built, from the context its config names', async () => {
  const probe = registryDouble({ published: new Set([`boxlite-app-dev:${SHA}-console`]) })
  const outcomes = await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  assert.deepEqual(
    outcomes.map((outcome) => [outcome.artifact, outcome.built]),
    [
      ['console', false],
      ['api', true],
    ],
  )
  const builds = probe.ran((call) => call[1] === 'build')
  assert.equal(builds.length, 1)
  assert.ok(builds[0]!.includes('/repo/apps/api/Dockerfile'), builds[0]!.join(' '))
  assert.ok(builds[0]!.includes(`REVISION=${SHA}`), 'the commit reaches the image as a build argument')
})

test('the commands that take minutes are echoed to the log, and nothing else is', async () => {
  // A step whose log stays empty for ten minutes cannot be told from a hung
  // one. Only these: `ecr get-login-password` prints a registry password and
  // the `describe-*` calls print documents to parse.
  const probe = registryDouble()
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  assert.deepEqual(
    probe.echoed.map((call) => `${call[0]} ${call[1]}`),
    ['docker build', 'docker push', 'docker build', 'docker push'],
  )
})

test('the log names each artifact as its build starts, not only once it is pushed', async () => {
  const probe = registryDouble()
  lines.length = 0
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  assert.deepEqual(
    lines.filter((line) => line.startsWith('Building')).map((line) => line.split(' ')[1]),
    ['console', 'api'],
  )
})

test('a publish addresses the stage it was given, not a default one', async () => {
  const probe = registryDouble()
  const outcomes = await publish({ config, stage: 'prod', registry: prod, tag: SHA, run: probe.run, log })
  assert.ok(outcomes.every((outcome) => outcome.address.includes('.ecr.us-east-1.') && outcome.address.includes('/boxlite-app-prod:')))
})

test('a repository that does not exist yet is created immutable and scanning', async () => {
  const probe = registryDouble({ repositories: new Set() })
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  const created = probe.ran((call) => call[2] === 'create-repository')[0]!
  assert.ok(created.includes('IMMUTABLE'), 'a mutable tag could be repointed under a running service')
  assert.ok(created.includes('scanOnPush=true'), 'the scan gate needs something to read')
  assert.ok(created.includes('ap-southeast-1'), "created in the stage's own region")
})

test('a blocking finding fails, naming what it found', async () => {
  const probe = registryDouble({ findings: { CRITICAL: 2, HIGH: 1, MEDIUM: 9 } })
  await assert.rejects(
    () => publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log }),
    /has 2 CRITICAL and 1 HIGH findings/,
  )
})

test('a scan refusal is told apart from a transient failure, so a caller stops retrying', async () => {
  // The distinction is the whole point: a push and a token endpoint fail
  // transiently and are worth a second attempt, and this is not — the answer is
  // about the image's own contents and a second ask returns it again. The
  // workflow reads it as an exit code (`bin/mbuild.ts`); this is the type it
  // reads it from.
  const refused = registryDouble({ findings: { CRITICAL: 1 } })
  await assert.rejects(
    () => publish({ config, stage: 'dev', registry: dev, tag: SHA, run: refused.run, log }),
    (error) => error instanceof ScanRefusedError,
  )

  // And a genuinely transient one is not: a failed push has to stay retryable,
  // or the distinction would have made everything unretryable instead.
  const broken = registryDouble({ pushFails: 'connection reset by peer' })
  await assert.rejects(
    () => publish({ config, stage: 'dev', registry: dev, tag: SHA, run: broken.run, log }),
    (error) => error instanceof PublishError && !(error instanceof ScanRefusedError),
  )
})

test('a scan that is not registered yet is waited for rather than failed on', async () => {
  // ECR registers the scan after the push returns, so the first read of a
  // just-pushed image finds no scan at all. Failing there fails a publish whose
  // image is fine, and leaves the workflow's own retry to pass it a minute
  // later on the run that finds everything already published.
  const probe = registryDouble({ scan: { missingReads: 2 } })
  const { clock, waits } = fakeClock()
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log, clock })
  const reads = probe.ran((call) => call[2] === 'describe-image-scan-findings')
  assert.equal(reads.length, 6, 'two reads that found no scan, then one that did, for each artifact')
  assert.deepEqual(waits, [5_000, 5_000, 5_000, 5_000], 'a poll interval between reads, never after the last')
})

test('a scan still running is not read as a clean image', async () => {
  // An unfinished scan carries no counts, and no counts read as nothing found:
  // the gate would pass an image that nothing has finished scanning.
  const probe = registryDouble({ scan: { status: 'IN_PROGRESS' } })
  const { clock, waits } = fakeClock()
  const said: string[] = []
  await assert.rejects(
    () =>
      publish({
        config: briefBudget,
        stage: 'dev',
        registry: dev,
        tag: SHA,
        run: probe.run,
        log: (line) => said.push(line),
        clock,
      }),
    /no scan result after 10s/,
  )
  assert.deepEqual(waits, [5_000, 5_000], 'the budget is spent, and not exceeded')
  assert.equal(probe.ran((call) => call[2] === 'describe-image-scan-findings').length, 3, 'one read per interval')
  assert.deepEqual(
    said.filter((line) => line.startsWith('Waiting')),
    [`Waiting up to 10s for the scan of ${dev.host}/boxlite-app-dev:${SHA}-api (IN_PROGRESS)`],
    'the wait is announced once, not once per poll',
  )
})

test('a budget shorter than one poll interval still buys a second read', async () => {
  // The wait is what is left of the budget rather than a fixed step, so a
  // three-second budget asks again after three seconds instead of giving up
  // having waited for nothing.
  const probe = registryDouble({ scan: { missingReads: 1 } })
  const { clock, waits } = fakeClock()
  await publish({ config: withScanBudget(3), stage: 'dev', registry: dev, tag: SHA, run: probe.run, log, clock })
  assert.deepEqual(waits, [3_000])
  assert.equal(probe.ran((call) => call[2] === 'describe-image-scan-findings').length, 2)
})

test('a continuously scanned image is answered by its ACTIVE report, not waited on', async () => {
  // Enhanced scanning is a registry-wide setting, so the same repository can
  // start reporting ACTIVE without this config changing. Reading that as
  // unfinished would spend the whole budget on findings that are current.
  const probe = registryDouble({ scan: { status: 'ACTIVE' }, findings: { CRITICAL: 1 } })
  const { clock, waits } = fakeClock()
  await assert.rejects(
    () => publish({ config: briefBudget, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log, clock }),
    /has 1 CRITICAL findings/,
  )
  assert.deepEqual(waits, [], 'an ACTIVE report is an answer, not something to wait for')
})

test('a scan ECR will not run fails with the reason it gave, not with a timeout', async () => {
  const probe = registryDouble({
    scan: { status: 'UNSUPPORTED_IMAGE', description: 'The operating system is not supported' },
  })
  const { clock, waits } = fakeClock()
  await assert.rejects(
    () => publish({ config: briefBudget, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log, clock }),
    /UNSUPPORTED_IMAGE: The operating system is not supported/,
  )
  assert.deepEqual(waits, [], 'a scan that will never run is not waited for')
})

test('the gate reads the scan status alongside the counts, as JSON', async () => {
  // Counts alone cannot be read: absent counts mean "clean" once the scan is
  // COMPLETE and "not scanned yet" until then. And `--output text` dies on a
  // response that carries no findings object, which is the state being waited
  // through.
  const probe = registryDouble()
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  const read = probe.ran((call) => call[2] === 'describe-image-scan-findings')[0]!
  const query = read[read.indexOf('--query') + 1]!
  assert.match(query, /imageScanStatus\.status/)
  assert.match(query, /imageScanFindings\.findingSeverityCounts/)
  assert.equal(read[read.indexOf('--output') + 1], 'json')
})

test('the scan gate runs after every push, not between them', async () => {
  // Pushing one artifact and then failing on the other's findings would leave
  // half a release published with no record of why the rest is missing.
  const probe = registryDouble()
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  const order = probe.calls.map((call) => `${call[0]} ${call[1]} ${call[2] ?? ''}`.trim())
  assert.ok(
    order.indexOf('aws ecr describe-image-scan-findings') > order.lastIndexOf('docker push'),
    'every push precedes the first scan',
  )
})

test('promote moves the built bytes rather than building them again', async () => {
  const probe = registryDouble({ published: both('boxlite-app-dev') })
  const outcomes = await promote({
    config,
    tag: SHA,
    from: { stage: 'dev', registry: dev },
    to: { stage: 'prod', registry: prod },
    run: probe.run,
    log,
  })
  assert.equal(probe.ran((call) => call[1] === 'build').length, 0, 'rebuilding could produce different bytes')
  assert.deepEqual(
    outcomes.map((outcome) => outcome.artifact),
    ['console', 'api'],
  )
  const api = outcomes.find((outcome) => outcome.artifact === 'api')!
  assert.ok(api.from.includes('.ecr.ap-southeast-1.') && api.from.includes('/boxlite-app-dev:'))
  assert.ok(api.address.includes('.ecr.us-east-1.') && api.address.includes('/boxlite-app-prod:'))
  // Pull, re-tag, push: ECR shares no layers between repositories.
  assert.deepEqual(
    probe.ran((call) => call[0] === 'docker' && ['pull', 'tag', 'push'].includes(call[1]!)).map((call) => call[1]),
    ['pull', 'tag', 'push', 'pull', 'tag', 'push'],
  )
})

test('a promotion echoes its transfers, which are the part that moves whole images', async () => {
  const probe = registryDouble({ published: both('boxlite-app-dev') })
  await promote({
    config,
    tag: SHA,
    from: { stage: 'dev', registry: dev },
    to: { stage: 'prod', registry: prod },
    run: probe.run,
    log,
  })
  assert.deepEqual(
    probe.echoed.map((call) => `${call[0]} ${call[1]}`),
    ['docker pull', 'docker push', 'docker pull', 'docker push'],
    'a re-tag is instant; the pull and the push are not',
  )
})

test('a source missing any artifact promotes nothing at all', async () => {
  // Half a release in prod is a version that cannot start.
  const probe = registryDouble({ published: new Set([`boxlite-app-dev:${SHA}-console`]) })
  await assert.rejects(
    () =>
      promote({
        config,
        tag: SHA,
        from: { stage: 'dev', registry: dev },
        to: { stage: 'prod', registry: prod },
        run: probe.run,
        log,
      }),
    /dev does not hold api at .*; nothing was promoted/,
  )
  assert.equal(probe.ran((call) => call[1] === 'pull').length, 0)
})

test('an artifact already in the destination is not moved twice', async () => {
  const probe = registryDouble({
    published: new Set([...both('boxlite-app-dev'), `boxlite-app-prod:${SHA}-console`]),
  })
  const outcomes = await promote({
    config,
    tag: SHA,
    from: { stage: 'dev', registry: dev },
    to: { stage: 'prod', registry: prod },
    run: probe.run,
    log,
  })
  assert.deepEqual(
    outcomes.map((outcome) => [outcome.artifact, outcome.built]),
    [
      ['console', false],
      ['api', true],
    ],
  )
  assert.equal(probe.ran((call) => call[1] === 'pull').length, 1)
})

test('promoting a stage to itself is refused rather than quietly doing nothing', async () => {
  await assert.rejects(
    () =>
      promote({
        config,
        tag: SHA,
        from: { stage: 'dev', registry: dev },
        to: { stage: 'dev', registry: dev },
        run: registryDouble().run,
        log,
      }),
    /Promoting "dev" to itself would do nothing/,
  )
})

test("the destination's scan decides whether the receiving stage may run it", async () => {
  const probe = registryDouble({ published: both('boxlite-app-dev'), findings: { CRITICAL: 1 } })
  await assert.rejects(
    () =>
      promote({
        config,
        tag: SHA,
        from: { stage: 'dev', registry: dev },
        to: { stage: 'prod', registry: prod },
        run: probe.run,
        log,
      }),
    /has 1 CRITICAL findings/,
  )
})

test('a failed build stops the publish rather than pushing nothing', async () => {
  const probe = registryDouble()
  const run: Run = async (command, args) =>
    command === 'docker' && args[0] === 'build' ? fail('no space left on device') : probe.run(command, args)
  await assert.rejects(() => publish({ config, stage: 'dev', registry: dev, tag: SHA, run, log }), PublishError)
})

/** A registry that answers like Artifact Registry, keyed by what is published. */
const googleDouble = ({
  published = new Set<string>(),
  repositories = new Set(['boxlite']),
  vulnerabilities = {} as Record<string, unknown[]>,
} = {}) => {
  const calls: string[][] = []
  const run: Run = async (command, args) => {
    calls.push([command, ...args])
    // A pushed image is a published one, which is what the scan gate then reads.
    if (command === 'docker' && args[0] === 'push') published.add(args[1]!)
    if (command === 'docker') return ok()
    if (args[0] === 'auth') return ok()
    const operation = args.slice(0, 3).join(' ')
    if (operation === 'artifacts repositories describe') {
      return repositories.has(args[3]!) ? ok() : fail('NOT_FOUND: repository')
    }
    if (operation === 'artifacts repositories create') return ok()
    if (operation === 'artifacts docker images') {
      const image = args[4]!
      if (!published.has(image)) return fail('NOT_FOUND: image')
      return args.includes('--show-package-vulnerability')
        ? ok(JSON.stringify({ package_vulnerability_summary: { vulnerabilities } }))
        : ok()
    }
    return ok()
  }
  return { calls, run, ran: (predicate: (call: string[]) => boolean) => calls.filter(predicate) }
}

const gcpConfig = parseBuildConfig(
  '/repo/apps/infra/mbuild.config.json',
  JSON.stringify({
    root: '../..',
    artifacts: { api: { dockerfile: 'apps/api/Dockerfile', context: '.' } },
    scan: { blockOn: ['CRITICAL'], timeoutSeconds: 300 },
    stages: {
      dev: {
        registry: {
          kind: 'artifact-registry',
          repository: 'boxlite',
          immutableTags: true,
          scanOnPush: true,
        },
      },
    },
  }),
)

const gar = resolveRegistry({ config: gcpConfig, stage: 'dev', region: 'asia-southeast1', project: 'boxlite' })
const GAR_IMAGE = `asia-southeast1-docker.pkg.dev/boxlite/boxlite/api:${SHA}`

test('a commit publishes to Artifact Registry, at the address that cloud uses', async () => {
  // One repository per artifact and the commit as the tag, rather than one
  // repository and a compound tag. `addressFor` already writes both shapes.
  const probe = googleDouble()
  const outcomes = await publish({ config: gcpConfig, stage: 'dev', registry: gar, tag: SHA, run: probe.run, log })
  assert.deepEqual(
    outcomes.map(({ artifact, address }) => [artifact, address]),
    [['api', GAR_IMAGE]],
  )
  assert.ok(probe.ran((call) => call[0] === 'docker' && call[1] === 'push' && call[2] === GAR_IMAGE).length === 1)
})

test('logging in configures the credential helper, and fetches no password', async () => {
  // Nothing expires between this and the push: the helper reads the ambient
  // credentials each time it is asked.
  const probe = googleDouble()
  await publish({ config: gcpConfig, stage: 'dev', registry: gar, tag: SHA, run: probe.run, log })
  assert.deepEqual(probe.ran((call) => call[1] === 'auth')[0]?.slice(0, 4), [
    'gcloud',
    'auth',
    'configure-docker',
    'asia-southeast1-docker.pkg.dev',
  ])
  assert.equal(probe.ran((call) => call.includes('get-login-password')).length, 0)
})

test('a repository that does not exist yet is created as a docker repository', async () => {
  const probe = googleDouble({ repositories: new Set() })
  await publish({ config: gcpConfig, stage: 'dev', registry: gar, tag: SHA, run: probe.run, log })
  const created = probe.ran((call) => call.slice(1, 4).join(' ') === 'artifacts repositories create')[0]!
  assert.ok(created.includes('--repository-format'))
  assert.ok(created.includes('docker'))
  assert.ok(created.includes('asia-southeast1'), created.join(' '))
})

test('a commit already published is not rebuilt on this cloud either', async () => {
  const probe = googleDouble({ published: new Set([GAR_IMAGE]) })
  const outcomes = await publish({ config: gcpConfig, stage: 'dev', registry: gar, tag: SHA, run: probe.run, log })
  assert.deepEqual(outcomes, [{ artifact: 'api', address: GAR_IMAGE, built: false }])
  assert.equal(probe.ran((call) => call[1] === 'build').length, 0)
})

test('vulnerability occurrences are counted by severity, which is what the gate reads', async () => {
  // Artifact Analysis answers one entry per occurrence; ECR answers counts. The
  // gate takes counts, so the translation happens once, here.
  const probe = googleDouble({
    published: new Set([GAR_IMAGE]),
    vulnerabilities: { CRITICAL: [{}, {}], LOW: [{}] },
  })
  await assert.rejects(
    () => publish({ config: gcpConfig, stage: 'dev', registry: gar, tag: SHA, run: probe.run, log }),
    /has 2 CRITICAL findings/,
  )
})

test('a clean image passes the same gate', async () => {
  const probe = googleDouble({ published: new Set([GAR_IMAGE]), vulnerabilities: { LOW: [{}] } })
  await assert.doesNotReject(() =>
    publish({ config: gcpConfig, stage: 'dev', registry: gar, tag: SHA, run: probe.run, log }),
  )
})

test('a build is given paths resolved from the repository, not from the working directory', async () => {
  // mbuild.config.json lives beside mstage.config.json in apps/infra, while the
  // Dockerfiles it names live at the repository root. Handing docker the
  // declared strings makes `apps/console/Dockerfile` mean
  // `apps/infra/apps/console/Dockerfile`, which is nothing.
  const probe = registryDouble()
  await publish({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run, log })
  const build = probe.ran((call) => call[0] === 'docker' && call[1] === 'build')[0]!
  assert.equal(build[build.indexOf('-f') + 1], '/repo/apps/console/Dockerfile')
  assert.equal(build.at(-1), '/repo', 'the context is the repository root, not "."')
})

test('the repository file resolves to Dockerfiles that exist', async () => {
  // Not a fixture: the real mbuild.config.json, so a `root` that stops pointing
  // at the repository is caught here rather than by a build that cannot find
  // its own Dockerfile.
  const path = new URL('../../mbuild.config.json', import.meta.url)
  const real = parseBuildConfig(path.pathname, readFileSync(path, 'utf8'))
  for (const [artifact, { dockerfile, context }] of Object.entries(real.artifacts)) {
    assert.ok(existsSync(join(real.repository, dockerfile)), `${artifact}: ${dockerfile}`)
    assert.ok(existsSync(join(real.repository, context)), `${artifact} context: ${context}`)
  }
})

test('a config that does not say where the repository is refuses to load', async () => {
  // Guessing would mean guessing wrong once, silently, in whichever direction
  // the caller happened to be standing.
  assert.throws(
    () =>
      parseBuildConfig(
        '/repo/apps/infra/mbuild.config.json',
        JSON.stringify({ artifacts: {}, scan: { blockOn: [], timeoutSeconds: 1 }, stages: {} }),
      ),
    /must set root/,
  )
})

test('a stage missing any image is refused, named by the address that was looked for', async () => {
  // The address rather than the artifact name: it carries the repository and
  // the tag, which is what a person compares against the publish that was
  // supposed to have written them.
  const probe = registryDouble({ published: new Set([`boxlite-app-dev:${SHA}-console`]) })
  await assert.rejects(
    () => verifyPublished({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run }),
    new RegExp(`dev does not hold \\S+/boxlite-app-dev:${SHA}-api$`),
  )
})

test('verifying reads the registry and does nothing else to it', async () => {
  // It runs before a deploy has committed to anything, so it must not create a
  // repository, log in, or build. Anything it changed would be one more thing a
  // refused deploy had already done.
  const probe = registryDouble({ repositories: new Set() })
  await assert.rejects(() => verifyPublished({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run }))
  assert.deepEqual(
    [...new Set(probe.calls.map((call) => `${call[0]} ${call[1]} ${call[2] ?? ''}`.trim()))],
    ['aws ecr describe-images'],
  )
})

test('verifying reports the address a runtime pulls, for every declared artifact', async () => {
  // The strings the deploy resolves through the same function. A check that
  // agreed on the answer but not on the address would pass against an image
  // nothing pulls.
  const probe = registryDouble({ published: both('boxlite-app-dev') })
  assert.deepEqual(await verifyPublished({ config, stage: 'dev', registry: dev, tag: SHA, run: probe.run }), [
    {
      artifact: 'console',
      address: `${ACCOUNT}.dkr.ecr.ap-southeast-1.amazonaws.com/boxlite-app-dev:${SHA}-console`,
    },
    { artifact: 'api', address: `${ACCOUNT}.dkr.ecr.ap-southeast-1.amazonaws.com/boxlite-app-dev:${SHA}-api` },
  ])
})
