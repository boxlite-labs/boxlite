/**
 * One place that decides which stage and region a command acts on.
 *
 * Which credentials answer for that account is deliberately not decided here.
 * The AWS SDK's own chain already reads `AWS_PROFILE`, `login_session`, SSO
 * caches, `credential_process`, container credentials and IMDS, in an order the
 * whole ecosystem agrees on. mstage adding a competing opinion is what produced
 * SST's bug, where `providers.aws.profile` loses to an ambient `AWS_PROFILE`
 * (pkg/project/provider/aws.go:76) and needs `SST_AWS_NO_PROFILE` as a third
 * knob. mstage does not choose credentials, and it does not second-guess which
 * account they reach: the account is whatever the chain resolves to.
 *
 * Region is different, and is decided here: a missing one silently becomes
 * us-east-1 in SST (aws.go:88, aws.go:108), which reads an empty bucket in the
 * wrong region rather than failing.
 */

import { ConfigError, homeFor, type Cloud, type MstageConfig, type StageConfig } from '../config/load.ts'
import type { Options } from '../cli/argv.ts'

export type Scope = {
  stage: string | null
  stageSource?: string
  protect: boolean
  /**
   * Which cloud this stage lives in, resolved from the stage's declaration or
   * the repository's default. Carried on the scope rather than looked up again
   * because every consumer — the store, the identity, the engine, the registry
   * — needs the same answer, and a second lookup is a second chance to differ.
   */
  home: Cloud
  /** The GCP project the stage declares. Null on AWS, which has no use for it. */
  project: string | null
  app: string | null
  appSource?: string
  region: string
  regionSource: string
  roleArn: string | null
  roleArnSource: string | null
  roleSessionName: string | null
}

type Candidate = [string, unknown]

export class ScopeError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ScopeError'
  }
}

const first = (candidates: Candidate[]): Candidate | undefined =>
  candidates.find(([, value]) => value !== undefined && value !== null && value !== '')

const resolveStage = (options: Options, config: MstageConfig, environment: NodeJS.ProcessEnv) => {
  const chosen = first([
    ['--stage', options.stage],
    ['MSTAGE_STAGE', environment.MSTAGE_STAGE],
  ])
  if (!chosen) {
    const known = Object.keys(config.stages).join(', ')
    throw new ScopeError(`--stage is required. ${config.path} declares: ${known}`)
  }
  const [source, name] = chosen as [string, string]
  if (!(name in config.stages)) {
    const known = Object.keys(config.stages).join(', ')
    throw new ScopeError(
      `Stage "${name}" (from ${source}) is not declared in ${config.path}. Declared stages: ${known}`,
    )
  }
  return { name, source, ...(config.stages[name] as StageConfig) }
}

const resolveRegion = (
  options: Options,
  stage: { name: string | null; region: string | null },
  environment: NodeJS.ProcessEnv,
) => {
  // The stage's declared region outranks the environment on purpose: a shell that
  // exports AWS_REGION for one stage must not quietly retarget another.
  const chosen = first([
    ['--region', options.region],
    [`${stage.name} in mstage.config.json`, stage.region],
    ['AWS_REGION', environment.AWS_REGION],
    ['AWS_DEFAULT_REGION', environment.AWS_DEFAULT_REGION],
  ])
  if (chosen) return { value: chosen[1] as string, source: chosen[0] }
  throw new ScopeError(
    `Could not determine an AWS region for stage "${stage.name}". ` +
      `Declare it as stages.${stage.name}.region in mstage.config.json, or pass --region.`,
  )
}

const resolveRole = (
  options: Options,
  stage: { name: string | null; roleArn: string | null },
  environment: NodeJS.ProcessEnv,
) => {
  const chosen = first([
    ['--role-arn', options['role-arn']],
    ['MSTAGE_AWS_ROLE_ARN', environment.MSTAGE_AWS_ROLE_ARN],
    [`${stage.name} in mstage.config.json`, stage.roleArn],
  ])
  if (!chosen) return { arn: null, source: null, sessionName: null }
  return {
    arn: chosen[1] as string,
    source: chosen[0],
    sessionName: (options['role-session-name'] as string) ?? environment.MSTAGE_AWS_ROLE_SESSION_NAME ?? 'mstage',
  }
}

const resolveApp = (options: Options, config: MstageConfig, environment: NodeJS.ProcessEnv) => {
  const chosen = first([
    ['--app', options.app],
    ['MSTAGE_APP', environment.MSTAGE_APP],
    [config.path, config.app],
  ])
  if (!chosen) throw new ConfigError(`${config.path} declares no "app" and none was given with --app`)
  return { value: chosen[1] as string, source: chosen[0] }
}

export const resolveScope = ({
  options,
  config,
  environment = process.env,
}: {
  options: Options
  config: MstageConfig
  environment?: NodeJS.ProcessEnv
}): Scope => {
  const stage = resolveStage(options, config, environment)
  const app = resolveApp(options, config, environment)
  const region = resolveRegion(options, stage, environment)
  const role = resolveRole(options, stage, environment)
  return {
    stage: stage.name,
    stageSource: stage.source,
    protect: stage.protect,
    home: homeFor(config, stage.name),
    project: stage.project,
    app: app.value,
    appSource: app.source,
    region: region.value,
    regionSource: region.source,
    roleArn: role.arn,
    roleArnSource: role.source,
    roleSessionName: role.sessionName,
  }
}
