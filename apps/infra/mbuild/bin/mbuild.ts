#!/usr/bin/env node

/*
 * `npm run mbuild publish -- --tag <sha> --stage dev`
 * `npm run mbuild promote -- --tag <sha> --from dev --to prod`
 * `npm run mbuild verify  -- --tag <sha> --stage dev`
 *
 * Credentials are the ambient ones: the caller has already federated a role
 * through OIDC, and asking for them again here would be a second source of
 * truth about who is publishing. The account is read once and used for both
 * registries, because a promotion between accounts would need a role for each
 * and nothing here would know which.
 *
 * Where a stage lives comes from mstage, which is the one file that declares it.
 * mbuild's own config says what to build and which repository receives it, and
 * deliberately does not repeat the region.
 */

import { loadConfig as loadStageConfig, type StageConfig } from 'mstage/config'
import { loadBuildConfig, registryFor, type BuildConfig } from '../src/config.ts'
import { resolveRegistry } from '../src/address.ts'
import { assertPromotable, coordinatesOf, type Coordinates } from '../src/coordinates.ts'
import { promote, publish, ScanRefusedError, verifyPublished } from '../src/publish.ts'
import { run } from '../src/run.ts'

/**
 * The exit code a scan refusal reports, and the only one that is not 1.
 *
 * A caller retries a publish because a push and a token endpoint fail
 * transiently. The scan gate does not: its answer is about the image's own
 * contents and a second ask returns it again. A distinct code is what lets the
 * shell in `.github/workflows/mbuild.yml` stop rather than spend two more
 * attempts re-reading the same findings.
 *
 * 78 is `EX_CONFIG` from `sysexits.h` — "something was wrong in the input", as
 * opposed to a failure of this program. Any code outside 1-2 would do; a named
 * one from a standard beats a number chosen here.
 */
const SCAN_REFUSED_EXIT = 78

const USAGE = [
  'usage: npm run mbuild publish -- --tag <commit-sha> --stage <stage>',
  '       npm run mbuild promote -- --tag <commit-sha> --from <stage> --to <stage>',
  '       npm run mbuild verify -- --tag <commit-sha> --stage <stage>',
  '       npm run mbuild inspect -- --stage <stage>',
].join('\n')

const option = (argv: string[], name: string): string | undefined => {
  const inline = argv.find((argument) => argument.startsWith(`--${name}=`))
  if (inline) return inline.slice(name.length + 3)
  const index = argv.indexOf(`--${name}`)
  return index >= 0 ? argv[index + 1] : undefined
}

const required = (argv: string[], name: string): string => {
  const value = option(argv, name)
  if (!value) throw new Error(`--${name} is required.\n${USAGE}`)
  return value
}

const accountId = async (): Promise<string> => {
  const identity = await run('aws', ['sts', 'get-caller-identity', '--query', 'Account', '--output', 'text'])
  if (identity.code !== 0) throw new Error(`Could not resolve the publishing account: ${identity.stderr.trim()}`)
  return identity.stdout.trim()
}

/** mstage declares where every stage lives; this reads that rather than a copy. */
const stageOf = (stage: string): StageConfig => {
  const stages = loadStageConfig().stages
  const declared = stages[stage]
  if (!declared) {
    throw new Error(`mstage.config.json declares no stage "${stage}". Declared: ${Object.keys(stages).join(', ')}`)
  }
  return declared
}

const regionOf = (stage: string): string => {
  const { region } = stageOf(stage)
  if (!region) throw new Error(`mstage.config.json gives stage "${stage}" no region`)
  return region
}

/**
 * What a stage publishes to, before anything has been federated.
 *
 * A caller that has to choose a cloud login cannot choose it until it knows
 * which cloud, and reading that from the config in shell would mean a second
 * parser for a file mbuild already owns. Deliberately identity-free: it names
 * the kind and the region, never the account or the project, because those are
 * exactly what the login it is choosing will establish.
 *
 * Printed as `key=value` lines, which is what a GitHub step reads into its
 * outputs without a transformation nobody would review.
 */
export const inspectStage = ({ config, stage, region }: { config: BuildConfig; stage: string; region: string }) => {
  const declared = registryFor(config, stage)
  return {
    kind: declared.kind,
    region,
    repository: declared.repository,
    artifacts: Object.keys(config.artifacts).join(','),
  }
}

/*
 * Progress goes where the echoed commands go, so the two stay in order: one
 * stream, read as it is written. stdout is left for what the run produced —
 * the `artifact=address` lines a caller can read.
 */
const log = (line: string): void => void process.stderr.write(`${line}\n`)

/** The bin's half of it: which stage, and where to ask for each half. */
const coordinates = (config: BuildConfig, stage: string): Promise<Coordinates> =>
  coordinatesOf({ stage, kind: registryFor(config, stage).kind, project: stageOf(stage).project, accountId })

const main = async (): Promise<number> => {
  const [command, ...argv] = process.argv.slice(2)
  const config = loadBuildConfig()

  if (command === 'inspect') {
    const stage = required(argv, 'stage')
    for (const [key, value] of Object.entries(inspectStage({ config, stage, region: regionOf(stage) }))) {
      console.log(`${key}=${value}`)
    }
    return 0
  }

  if (command === 'publish') {
    const tag = required(argv, 'tag')
    const stage = required(argv, 'stage')
    const registry = resolveRegistry({
      config,
      stage,
      region: regionOf(stage),
      ...(await coordinates(config, stage)),
    })
    for (const outcome of await publish({ config, stage, registry, tag, run, log })) {
      console.log(`${outcome.artifact}=${outcome.address}`)
    }
    return 0
  }

  /*
   * Asked by a deploy before it applies anything. It prints the same
   * `artifact=address` lines a publish does, because a caller that wants to
   * know where the images are should not have to read a different shape
   * depending on whether this run put them there.
   */
  if (command === 'verify') {
    const tag = required(argv, 'tag')
    const stage = required(argv, 'stage')
    const registry = resolveRegistry({
      config,
      stage,
      region: regionOf(stage),
      ...(await coordinates(config, stage)),
    })
    for (const { artifact, address } of await verifyPublished({ config, stage, registry, tag, run })) {
      console.log(`${artifact}=${address}`)
    }
    return 0
  }

  if (command === 'promote') {
    const tag = required(argv, 'tag')
    const fromStage = required(argv, 'from')
    const toStage = required(argv, 'to')
    assertPromotable({
      from: { stage: fromStage, kind: registryFor(config, fromStage).kind },
      to: { stage: toStage, kind: registryFor(config, toStage).kind },
    })
    // Resolved per stage, not once from the source. On Artifact Registry the
    // project is part of the address, and two stages may live in two projects —
    // reusing the source's would push the promoted image at the wrong one while
    // reporting the right name.
    const outcomes = await promote({
      config,
      tag,
      from: {
        stage: fromStage,
        registry: resolveRegistry({
          config,
          stage: fromStage,
          region: regionOf(fromStage),
          ...(await coordinates(config, fromStage)),
        }),
      },
      to: {
        stage: toStage,
        registry: resolveRegistry({
          config,
          stage: toStage,
          region: regionOf(toStage),
          ...(await coordinates(config, toStage)),
        }),
      },
      run,
      log,
    })
    for (const outcome of outcomes) console.log(`${outcome.artifact}=${outcome.address}`)
    return 0
  }

  console.error(command ? `Unknown command "${command}".\n${USAGE}` : USAGE)
  return 1
}

try {
  process.exitCode = await main()
} catch (error) {
  console.error(error instanceof Error ? error.message : String(error))
  process.exitCode = error instanceof ScanRefusedError ? SCAN_REFUSED_EXIT : 1
}
