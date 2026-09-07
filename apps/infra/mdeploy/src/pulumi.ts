/*
 * How a GCP stage deploys: Pulumi directly, with its state in the project it
 * deploys into.
 *
 * The AWS path spawns `sst`, and will keep doing that — its providers are
 * written against SST components. This path exists because of one thing SST
 * cannot do: keep state anywhere but S3, R2 or a local directory. Its `Home`
 * interface is unimplementable outside its own package (every method but
 * `Bootstrap` is unexported), and `LoadHome` is a three-case switch with
 * `default: invalid`. Pulumi's own backend takes `gs://`, so this driver gives a
 * GCP stage what the SST path cannot: no second cloud in the deploy path at all.
 *
 * The program runs inline rather than from a `Pulumi.yaml` — the stack is
 * TypeScript that already runs under this process's loader, and an inline
 * program keeps the engine from having to work out how to compile it. The cost
 * is that the program cannot read `process.env` for the stage's configuration,
 * which is why `gcpProgram` takes an environment instead.
 *
 * Targeting is deliberately refused here rather than translated. SST's
 * `--target` selects on logical component names and `plan.ts` lists the AWS
 * ones; Pulumi selects on URNs, and the GCP bundle's resources have different
 * names entirely (`Network`/`Subnetwork`/`Nat` where AWS has one `Vpc`). A
 * mapping invented here would silently deploy the wrong subset.
 */

import { gcpProgram, type ProgramOutputs } from '../pulumi/program.ts'
import type { Identity } from 'mstage/identity'
import type { MstageConfig } from 'mstage/config'
import type { StoreBackend } from 'mstage/env'
import type { Scope } from 'mstage/scope'
import { windowFor } from './credential-window.ts'
import { PULUMI_GROUP, TEARDOWN_GROUPS, rolloutGroups } from './env.ts'
import type { DeployRequest, DeployTarget, Intent } from './deploy.ts'

/** What each intent is called while it runs, and which engine call performs it. */
const NARRATION: Record<Intent, string> = { deploy: 'deploying', diff: 'comparing', remove: 'removing' }
const ACT: Record<Intent, (stack: PulumiStack, options: { onOutput: (output: string) => void }) => Promise<unknown>> = {
  deploy: (stack, options) => stack.up(options),
  diff: (stack, options) => stack.preview(options),
  remove: (stack, options) => stack.destroy(options),
}

export class PulumiDeployError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'PulumiDeployError'
  }
}

/**
 * The passphrase Pulumi seals the state's secrets with.
 *
 * Kept in the stage's own store, like everything else a deploy needs, rather
 * than in a cloud key service: mstage already distributes per-stage secrets, and
 * a second mechanism for one value would be a second thing to bootstrap.
 */
const PASSPHRASE_KEY = 'PULUMI_CONFIG_PASSPHRASE'

const AUTOMATION = '@pulumi/pulumi/automation'

/** The part of the Automation API this uses, in the shape it already has. */
export type PulumiStack = {
  setAllConfig: (config: Record<string, { value: string; secret?: boolean }>) => Promise<void>
  up: (options: { onOutput?: (output: string) => void }) => Promise<unknown>
  preview: (options: { onOutput?: (output: string) => void }) => Promise<unknown>
  destroy: (options: { onOutput?: (output: string) => void }) => Promise<unknown>
}

export type StackFactory = (input: {
  projectName: string
  stackName: string
  program: () => Promise<ProgramOutputs>
  /** What the engine subprocess inherits: credentials, and the passphrase. */
  envVars: Record<string, string>
  /** `gs://<bucket>` — the same bucket mstage keeps this stage's store in. */
  backendUrl: string
}) => Promise<PulumiStack>

/**
 * The real Automation API, imported only when a GCP stage asks for it.
 *
 * Structural above and lazy here for the reason mstage loads the Google SDKs
 * lazily (`mstage/src/home.ts:44-49`): an AWS-only repository should not have to
 * carry the packages, and a test should be able to drive this without them.
 */
const createStack: StackFactory = async ({ projectName, stackName, program, envVars, backendUrl }) => {
  let automation: any
  try {
    // A constant rather than a literal: see the note in `pulumi/program.ts`.
    automation = await import(AUTOMATION)
  } catch (error) {
    throw new PulumiDeployError(
      'A GCP stage needs @pulumi/pulumi installed in apps/infra, and the `pulumi` CLI on PATH — ' +
        'the Automation API does not bundle the engine, it spawns that binary. ' +
        `Install both, or pass a factory to pulumiDeploy. (${(error as Error).message})`,
    )
  }
  return automation.LocalWorkspace.createOrSelectStack(
    { stackName, projectName, program },
    {
      envVars,
      projectSettings: { name: projectName, runtime: 'nodejs', backend: { url: backendUrl } },
    },
  )
}

export type PulumiDeployInput = {
  intent?: Intent
  config: Pick<MstageConfig, 'root'>
  scope: Scope
  /**
   * Cloud-neutral on purpose: this asks the two questions every identity
   * answers, and never for a credential. The GCP identity has nothing to hand
   * over — Application Default Credentials are a file the SDK re-reads — so a
   * driver that asked for a key triple could not be given one.
   */
  identity: Pick<Identity, 'assertUsableFor' | 'childEnvironment'>
  /** Where mstage keeps this stage's store, and where Pulumi keeps its state. */
  state: { bucket: string }
  /** The stage's configuration, handed to the program rather than exported. */
  stageEnvironment?: Record<string, string>
  log: (line: string) => void
  createStackWith?: StackFactory
}

/** Nothing but strings reaches a subprocess; an unset variable is not an empty one. */
const definedOnly = (environment: NodeJS.ProcessEnv): Record<string, string> =>
  Object.fromEntries(Object.entries(environment).filter((entry): entry is [string, string] => entry[1] !== undefined))

export const pulumiDeploy = async ({
  intent = 'deploy',
  config,
  scope,
  identity,
  state,
  stageEnvironment = {},
  log,
  createStackWith = createStack,
}: PulumiDeployInput): Promise<number> => {
  /*
   * The project the stage declares, which is also what the Pulumi provider has
   * to be pointed at. Read from the one declaration rather than asked of the
   * credentials: mstage does not second-guess which tenant the resolved
   * credentials reach, and a GCP client cannot be built without a project
   * anyway (`mstage/src/config/load.ts`).
   */
  const project = scope.project
  if (!project) {
    throw new PulumiDeployError(
      `Stage "${scope.stage}" declares no project. A GCP stage declares it as project in mstage.config.json.`,
    )
  }
  // Nothing to check on GCP — Application Default Credentials refresh
  // themselves — but the AWS identity answers the same question, and a driver
  // that skipped it would be one place the two clouds differ for no reason.
  await identity.assertUsableFor(windowFor(intent))

  const { env: credentials } = await identity.childEnvironment()
  // The store last, so a stale export in this shell cannot shadow it: the point
  // of keeping configuration in one place is that it wins.
  const environment = definedOnly({ ...credentials, ...stageEnvironment })
  if (!environment[PASSPHRASE_KEY]?.trim()) {
    throw new PulumiDeployError(
      `${PASSPHRASE_KEY} is required — Pulumi seals the state's secrets with it. Generate 32 random bytes ` +
        "once and put them in this stage's store:\n" +
        `  openssl rand -base64 32 | npm run mstage env set -- --stage <stage> ${PASSPHRASE_KEY}\n` +
        'Never replace it afterwards: every secret already in the state was sealed with this value, and a ' +
        'new one leaves them permanently unreadable. See iam/README.md.',
    )
  }

  const app = scope.app as string
  const stage = scope.stage as string
  log(
    `${NARRATION[intent]} every component of ${app} stage ${stage} ` +
      `${intent === 'remove' ? 'from' : 'into'} ${project} (${scope.region}) with pulumi`,
  )

  const stack = await createStackWith({
    projectName: app,
    stackName: stage,
    program: gcpProgram({ app, stage, region: scope.region, project, environment, cwd: config.root }),
    /*
     * The whole stage environment, not only the credentials and the passphrase.
     * The engine spawns a provider plugin per provider, and those are the
     * processes that read `CLOUDFLARE_API_TOKEN` — narrowing this to what the
     * CLI itself needs would leave the DNS record unwritable.
     */
    envVars: environment,
    backendUrl: `gs://${state.bucket}`,
  })

  // The provider's coordinates as stack configuration rather than as
  // environment: an inline program is not a subprocess, so what the engine is
  // told is what it uses.
  await stack.setAllConfig({
    'gcp:project': { value: project },
    'gcp:region': { value: scope.region },
  })

  const onOutput = (output: string) => {
    for (const line of output.split('\n')) if (line.trim()) log(line)
  }
  try {
    await ACT[intent](stack, { onOutput })
    return 0
  } catch (error) {
    // The engine already printed what failed; this says which stage it was, so
    // a workflow log that scrolled past the failure still names it.
    log(`pulumi ${intent} failed for ${app} stage ${stage}: ${(error as Error).message}`)
    return 1
  }
}

export type GcpTargetInput = {
  config: Pick<MstageConfig, 'root'>
  scope: Scope
  identity: PulumiDeployInput['identity']
  /** Where this stage's configuration is read from, carried for the caller. */
  backend: StoreBackend
  /**
   * Asked at deploy time rather than resolved when the target is built. It is a
   * lookup against the bootstrap record, and `--plan`, a usage error or a
   * refused `--module` should not spend one.
   */
  stateBucket: () => Promise<string>
  createStackWith?: StackFactory
}

/**
 * A GCP stage, as one bundle.
 *
 * Two groups rather than one: the stage's own configuration, and the passphrase
 * Pulumi opens its state with. The SST path has no equivalent, which is exactly
 * why the group list belongs to the target instead of to whoever fetches it.
 */
export const gcpTarget = ({
  config,
  scope,
  identity,
  backend,
  stateBucket,
  createStackWith,
}: GcpTargetInput): DeployTarget => ({
  cloud: 'gcp',
  engine: 'pulumi',
  backend,
  // The passphrase on either intent: a teardown still has to open the state to
  // destroy what it describes.
  environmentGroups: (intent, declaration) => [
    ...(intent === 'remove' ? TEARDOWN_GROUPS : rolloutGroups(declaration)),
    PULUMI_GROUP,
  ],

  async run({ intent, targets = [], stageEnvironment, log }: DeployRequest): Promise<number> {
    /*
     * Refused rather than translated, and refused here because it is this
     * engine's limit rather than a fact about the cloud. `plan.ts` lists SST's
     * logical component names and Pulumi selects on URNs; the GCP bundle's
     * resources are not even named the same — `Network`, `Subnetwork`, `Nat`
     * where AWS has one `Vpc`. A mapping guessed here would deploy some other
     * subset and report success.
     */
    if (targets.length > 0) {
      throw new PulumiDeployError(
        `--module is not supported on a GCP stage yet: ${targets.join(', ')} are SST component names, ` +
          'and mdeploy/src/plan.ts has no GCP components to select on',
      )
    }
    return pulumiDeploy({
      intent,
      config,
      scope,
      identity,
      state: { bucket: await stateBucket() },
      stageEnvironment,
      log,
      ...(createStackWith ? { createStackWith } : {}),
    })
  },
})
