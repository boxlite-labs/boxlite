/*
 * The GCP stack, run by Pulumi rather than by SST.
 *
 * The modules under `mdeploy/stack/` name no cloud and no engine — they are
 * written against `StackProviders` and against a handful of names SST happens
 * to inject as globals. The GCP provider bundle uses no SST component at all:
 * every resource in it is a plain provider resource — `gcp.*`, plus `random.*`
 * for the two generated passwords and `cloudflare.*` for the DNS records — and
 * `$util` is `@pulumi/pulumi` under another name. So the only thing standing
 * between that bundle and a plain Pulumi program is who defines those names.
 *
 * This file defines them, and then runs the same `deployStack` the SST path
 * runs. Not one line of the stack changes, which is the point: two engines, one
 * description of BoxLite. The AWS path keeps SST because its providers do use
 * SST components — `sst.aws.Vpc`, `sst.aws.Cluster`, `sst.aws.Service` — and
 * nothing else can instantiate those.
 *
 * What this buys is the state backend. SST keeps state only in S3, R2 or a
 * directory on the machine, and its `Home` interface is unimplementable outside
 * its own package. Pulumi's own backend takes `gs://`, so a GCP stage can keep
 * its state in the project it deploys into and needs no other cloud's
 * credentials to deploy at all.
 */

import { readStackEnvironment } from '../src/stack-env.ts'
import type { deployStack } from '../stack/index.ts'

export class PulumiProgramError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'PulumiProgramError'
  }
}

/**
 * The Pulumi modules the stack needs, in the shape they already have.
 *
 * Structural rather than imported, for the same reason mstage describes the
 * Google clients structurally: a test should be able to drive the wiring
 * without the packages, and mdeploy floats to repositories that have not
 * adopted GCP.
 *
 * Four, not one. `gcp` is the obvious one; the other three are what the GCP
 * bundle also reaches for, and each was found by reading it rather than by
 * assuming. `random` generates the database and cache passwords, and
 * `cloudflare` writes the two DNS records — the zone is not Google's on either
 * cloud. A missing one is not a type error anywhere: the deploy reaches that
 * module and dies on `random is not defined`, halfway through an apply.
 */
export type PulumiModules = {
  pulumi: {
    interpolate: unknown
    output: unknown
    jsonStringify: unknown
    all: unknown
    secret: unknown
  }
  gcp: object
  random: object
  cloudflare: object
}

/** What a program returns to the engine: the stack's outputs, by name. */
export type ProgramOutputs = Record<string, unknown>

export type ModuleLoader = () => Promise<PulumiModules>

const PULUMI = '@pulumi/pulumi'
const PULUMI_GCP = '@pulumi/gcp'
const PULUMI_RANDOM = '@pulumi/random'
const PULUMI_CLOUDFLARE = '@pulumi/cloudflare'

/**
 * The real modules, imported only when a GCP stage asks for them.
 *
 * The specifiers are constants rather than literals on purpose. A literal would
 * make typechecking this file require all four packages to be installed, and
 * `@pulumi/gcp` is a large download that no AWS stage needs. Resolving them at
 * runtime, when a GCP stage is actually deploying, does not.
 */
const loadModules: ModuleLoader = async () => {
  try {
    const [pulumi, gcp, random, cloudflare] = await Promise.all([
      import(PULUMI),
      import(PULUMI_GCP),
      import(PULUMI_RANDOM),
      import(PULUMI_CLOUDFLARE),
    ])
    return { pulumi: pulumi as PulumiModules['pulumi'], gcp, random, cloudflare }
  } catch (error) {
    throw new PulumiProgramError(
      `A GCP stage needs ${[PULUMI, PULUMI_GCP, PULUMI_RANDOM, PULUMI_CLOUDFLARE].join(', ')} ` +
        `installed in apps/infra. Install them, or pass a loader to gcpProgram. (${(error as Error).message})`,
    )
  }
}

/**
 * Gives the stack the names SST would have injected.
 *
 * Globals rather than imports because that is what the stack already reads, and
 * rewriting every reference across a dozen modules to reach the same objects by
 * another name would be a change to the AWS path as well — a refactor with no
 * behaviour in it, on the one file set that deploys today.
 *
 * `$transform` and `sst` are deliberately left undefined. Their only callers
 * are the AWS role-boundary rule and the AWS providers' components, both of
 * which belong to the AWS engine; a GCP resource that reached for either should
 * fail loudly here rather than silently do nothing.
 */
export const installGlobals = ({
  modules,
  app,
  stage,
  target = globalThis as Record<string, unknown>,
}: {
  modules: PulumiModules
  app: string
  stage: string
  target?: Record<string, unknown>
}): void => {
  target.$app = { name: app, stage }
  target.$util = modules.pulumi
  target.$interpolate = modules.pulumi.interpolate
  target.$output = modules.pulumi.output
  target.$jsonStringify = modules.pulumi.jsonStringify
  target.$resolve = modules.pulumi.all
  target.gcp = modules.gcp
  target.random = modules.random
  target.cloudflare = modules.cloudflare
}

export type ProgramInput = {
  /** The app and stage this deploy is for. Both name the state it adopts. */
  app: string
  stage: string
  /** Where the stage lives. `mstage.config.json` declares it; nothing repeats it. */
  region: string
  /** The project this stage deploys into, which mstage also pins its store to. */
  project: string
  /**
   * The stage's configuration, handed in rather than read from `process.env`.
   * An inline Pulumi program runs inside the driver's own process, so reading
   * the ambient environment would read whatever that shell happened to hold —
   * the store is the one place a stage's configuration comes from.
   */
  environment: NodeJS.ProcessEnv
  /** Where `mdeploy.config.json` is found. Defaults to the process's own directory. */
  cwd?: string
  loadModules?: ModuleLoader
  /**
   * The stack, injected for the same reason `loadModules` is: what this
   * function decides — the order the globals are installed in, which
   * declaration the environment is read against, and the inputs handed over —
   * is provable without building a network. The default is the real one,
   * imported below after the globals exist.
   */
  deployStackWith?: typeof deployStack
}

/**
 * One deploy, as the function Pulumi calls.
 *
 * The order mirrors `sst.config.ts`'s `run()` exactly, because it is the same
 * deploy: read what this run decides, choose the provider bundle, hand it to
 * the stack.
 */
export const gcpProgram =
  ({ app, stage, region, project, environment, cwd, loadModules: load = loadModules, deployStackWith }: ProgramInput) =>
  async (): Promise<ProgramOutputs> => {
    const modules = await load()
    installGlobals({ modules, app, stage })

    /*
     * Imported after the globals exist, not before. Nothing in these modules
     * reads a global while it is being evaluated today, and a static import
     * would work — but the ordering is load-bearing enough that saying it in
     * the code is cheaper than a comment asking the next person to preserve it.
     */
    const { loadDeployConfig } = await import('../src/config.ts')
    const { loadConfig: loadStageConfig } = await import('mstage/config')
    const { deployStack: realDeployStack } = await import('../stack/index.ts')
    const deployStack = deployStackWith ?? realDeployStack
    const { gcpStackProviders } = await import('../stack/providers/gcp/index.ts')

    const config = loadDeployConfig({ cwd })
    const stageConfig = loadStageConfig({ cwd })
    const stackEnvironment = readStackEnvironment({
      environment,
      // The one declaration of which keys are the service's, not the deploy's.
      declaration: {
        groups: stageConfig.envSelectGroup,
        optional: stageConfig.envOptional,
        where: stageConfig.path,
      },
      stage,
      region,
      home: 'gcp',
    })

    const outputs = deployStack({
      providers: gcpStackProviders({
        stage,
        region,
        project,
        domain: stackEnvironment.domain,
        zoneId: stackEnvironment.dnsZoneId,
        relayHost: stackEnvironment.mailRelayHost,
        managedClickHouse: stackEnvironment.managedClickHouse,
      }),
      config,
      inputs: {
        stage,
        tag: stackEnvironment.tag,
        domain: stackEnvironment.domain,
        proxyDomain: stackEnvironment.proxyDomain,
        proxyProtocol: stackEnvironment.proxyProtocol,
        internetEgress: true,
        senderDomain: stackEnvironment.senderDomain,
        runnerBinary: stackEnvironment.runnerBinary,
        runnerFleet: stackEnvironment.runnerFleet,
        apiEnvironment: stackEnvironment.apiEnvironment,
        apiSecrets: stackEnvironment.apiSecrets,
        proxyEnvironment: stackEnvironment.proxyEnvironment,
        proxySecrets: stackEnvironment.proxySecrets,
        collectorEnvironment: stackEnvironment.collectorEnvironment,
        collectorSecrets: stackEnvironment.collectorSecrets,
        runnerEnvironment: stackEnvironment.runnerEnvironment,
        runnerSecrets: stackEnvironment.runnerSecrets,
      },
    })

    return { ...outputs }
  }
