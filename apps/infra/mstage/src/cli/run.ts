/**
 * Dispatches one mstage invocation.
 *
 * Everything here is shared across repositories. Deployment is not: it belongs
 * to each repository's own tool, which calls mstage for a session and an identity
 * and then does its own work. mstage creates and verifies access; it never spends it.
 */

import { loadConfig, type MstageConfig } from '../config/load.ts'
import { resolveIdentity, type AwsIdentity } from '../aws/identity.ts'
import { resolveHome } from '../home.ts'
import type { Identity } from '../identity.ts'
import type { StoreBackend } from '../env/backend.ts'
import { resolveScope, type Scope } from '../aws/precedence.ts'
import { parseInvocation, type Options } from './argv.ts'
import { moduleUsage, type CommandSpec, type ModuleSpec } from './help.ts'
import * as aws from './handlers/aws.ts'
import * as env from './handlers/env.ts'
import * as state from './handlers/state.ts'
import { checkAws, report, type ProviderCheck, type ProviderStatus } from './handlers/login.ts'
import {
  SIGN_IN_COMMANDS,
  SIGN_OUT_COMMANDS,
  checkAuth0,
  checkGcp,
  checkGitHub,
  signIn,
  signOut,
  type SignInResult,
} from '../auth/sessions.ts'
import { confirm as askConfirm, isInteractive, type Confirm } from './prompt.ts'

export class UsageError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'UsageError'
  }
}

export type Log = (line: string) => void

const AWS_COMMANDS: Record<string, CommandSpec> = {
  whoami: {
    run: aws.whoami,
    summary: 'Which identity, account and region this stage resolves to',
    requires: ['stage'],
  },
  region: {
    run: aws.region,
    summary: 'The region this stage resolves to, and why',
    requires: ['stage'],
  },
  exec: {
    run: aws.exec,
    summary: 'Run another command under that identity',
    inner: 'required',
    requires: ['stage'],
  },
}

const ENV_COMMANDS: Record<string, CommandSpec> = {
  list: {
    run: env.list,
    summary: "A stage's environment, by name",
    requires: ['stage'],
    accepts: ['select-group', 'json', 'values', 'version'],
  },
  set: {
    run: env.set,
    summary: 'Set one or more keys in a stage',
    argument: {
      form: 'KEY=VALUE …',
      description: 'assignments; a lone KEY reads stdin',
      optional: 'piped JSON or --digest supplies them',
    },
    requires: ['stage'],
    accepts: ['confirm', 'digest', 'json', 'select-group'],
  },
  versions: {
    run: env.versions,
    summary: 'Every stored version of a stage, newest first',
    requires: ['stage'],
  },
  digest: {
    run: env.digest,
    summary: 'Check the stored fingerprint still describes its group',
    requires: ['stage'],
  },
  del: {
    run: env.del,
    summary: 'Remove one or more keys from a stage',
    argument: { form: 'KEY …', description: 'the names to remove' },
    requires: ['stage'],
    accepts: ['confirm', 'digest'],
  },
}

const STATE_COMMANDS: Record<string, CommandSpec> = {
  unlock: {
    run: state.unlock,
    summary: 'Drop the lock a stopped deploy left on this stage',
    requires: ['stage'],
    accepts: ['confirm'],
  },
  edit: {
    run: state.edit,
    summary: "Open this stage's deployment state in $EDITOR",
    requires: ['stage'],
    accepts: ['confirm'],
  },
}

/** Shared by every stage-scoped command, so they are listed once per module. */
const STAGE_OPTIONS = ['app', 'region', 'role-arn', 'role-session-name']

/**
 * Every provider mstage knows how to check. mstage.config.json selects which of them
 * a repository requires; it does not define the set, so all of them are
 * documented whether or not this repository has enabled them.
 */
const LOGIN_PROVIDERS: Record<string, { check: ProviderCheck; summary: string }> = {
  aws: { check: checkAws, summary: 'AWS credentials, through the SDK default chain' },
  gcp: { check: checkGcp as ProviderCheck, summary: 'Google application default credentials' },
  // These two read a CLI session and ignore the AWS identity in the context.
  github: { check: checkGitHub as ProviderCheck, summary: 'The gh CLI session, and which account it holds' },
  auth0: { check: checkAuth0 as ProviderCheck, summary: 'The auth0 CLI session, and its active tenant' },
}

const LOGIN_CHECKS: Record<string, ProviderCheck> = Object.fromEntries(
  Object.entries(LOGIN_PROVIDERS).map(([key, provider]) => [key, provider.check]),
)

const LOGIN_COMMANDS: Record<string, CommandSpec> = Object.fromEntries(
  Object.entries(LOGIN_PROVIDERS).map(([key, provider]) => [key, { summary: provider.summary }]),
)

const MODULES: Record<string, ModuleSpec> = {
  login: {
    summary: 'Check every sign-in this repository declares',
    scope: 'login',
    commands: LOGIN_COMMANDS,
    commandNote:
      'omit the command to act on every provider mstage.config.json enables;\n' +
      'naming one mstage.config.json does not enable is refused',
    accepts: ['force', 'logout', 'region'],
    example: 'npm run mstage login github -- --force',
  },
  aws: {
    summary: 'AWS identity for a stage',
    scope: 'stage',
    commands: AWS_COMMANDS,
    accepts: STAGE_OPTIONS,
    example: 'npm run mstage aws exec -- --stage=dev -- aws s3 ls',
  },
  env: {
    summary: "A stage's environment, read from the SST state bucket",
    scope: 'stage',
    commands: ENV_COMMANDS,
    accepts: STAGE_OPTIONS,
    example: 'npm run mstage env list -- --stage=dev --select-group=deploy --json > .deploy.env.json',
  },
  state: {
    summary: 'What a stopped deploy left in that bucket: a lock, and a checkpoint',
    scope: 'stage',
    commands: STATE_COMMANDS,
    commandNote:
      'a cancelled deploy leaves both: the lock it never released, and the\n' +
      'operations it was in the middle of, which the next deploy refuses to plan over',
    accepts: STAGE_OPTIONS,
    example: 'npm run mstage state unlock -- --stage=dev',
  },
}

/** Every module the dispatcher knows, named once so nothing keeps a second list. */
export const MODULE_NAMES = Object.keys(MODULES)

export const usage = (): string => {
  const lines = ['usage: npm run mstage <module> <command> -- [--stage <stage>] [options] [-- <inner command>]', '']
  lines.push('modules')
  for (const [name, module] of Object.entries(MODULES)) {
    lines.push(`  ${name.padEnd(10)}${module.summary}`)
    for (const [command, spec] of Object.entries(module.commands ?? {})) {
      lines.push(`    ${command.padEnd(8)}${spec.summary}`)
    }
  }
  lines.push('', 'login providers are declared per repository in mstage.config.json')
  lines.push('  -f, --force  sign in again first, instead of only reporting the current session')
  lines.push('      --logout  end the current session instead of checking it')
  lines.push('', 'every option must sit to the right of the "--"; npm claims anything left of it')
  return lines.join('\n')
}

const buildStageContext = async ({
  options,
  environment,
  cwd,
}: {
  options: Options
  environment: NodeJS.ProcessEnv
  cwd: string
}): Promise<{ config: MstageConfig; scope: Scope; identity: Identity; backend: StoreBackend }> => {
  const config = loadConfig({ cwd, environment })
  const scope = resolveScope({ options, config, environment })
  // The one place a cloud is chosen. Everything below works against the two
  // interfaces and never learns which one answered.
  const { identity, backend } = await resolveHome({ scope })
  return { config, scope, identity, backend }
}

// `login` answers "can mstage reach AWS", which is not a per-stage question. STS
// still needs a region to build its client; the value only picks an endpoint and
// is never reported, because it describes nothing.
const STS_ENDPOINT_REGION_FALLBACK = 'us-east-1'

const buildLoginContext = ({
  options,
  environment,
}: {
  options: Options
  environment: NodeJS.ProcessEnv
}): { identity: AwsIdentity } => {
  const region =
    (options.region as string) ??
    environment.AWS_REGION ??
    environment.AWS_DEFAULT_REGION ??
    STS_ENDPOINT_REGION_FALLBACK
  return { identity: resolveIdentity({ scope: { region, roleArn: null } as Scope }) }
}

const runLogin = async ({
  command,
  options,
  environment,
  cwd,
  log,
  signInWith,
  signOutWith,
  confirm,
  interactive,
  checks,
}: {
  command: string | null
  options: Options
  environment: NodeJS.ProcessEnv
  cwd: string
  log: Log
  signInWith: (provider: string) => SignInResult
  signOutWith: (provider: string) => SignInResult
  confirm: Confirm
  interactive: boolean
  checks: Record<string, ProviderCheck>
}): Promise<number> => {
  const declared = loadConfig({ cwd, environment }).login
  if (Object.keys(declared).length === 0) {
    throw new UsageError('mstage.config.json declares no "login" providers, so there is nothing to check')
  }
  for (const provider of Object.keys(declared)) {
    if (!(provider in checks)) {
      throw new UsageError(
        `mstage.config.json declares "${provider}", which mstage cannot check. Known: ${Object.keys(checks).join(', ')}`,
      )
    }
  }
  const wanted = command === null ? Object.keys(declared) : [command]
  for (const provider of wanted) {
    if (!(provider in declared)) {
      throw new UsageError(
        `This repository does not use "${provider}". mstage.config.json declares: ${Object.keys(declared).join(', ')}`,
      )
    }
  }

  if (options.logout === true && options.force === true) {
    throw new UsageError('--logout and --force ask for opposite things; pass one')
  }

  // Signing out first, so the report that follows describes what is left rather
  // than what was there. Nothing is offered afterwards: someone who just asked
  // to sign out does not want to be asked to sign back in.
  if (options.logout === true) {
    for (const provider of wanted) {
      log(`${provider.padEnd(8)}signing out: ${SIGN_OUT_COMMANDS[provider]?.join(' ')}`)
      const attempt = signOutWith(provider)
      if (!attempt.ok) log(`        ${attempt.detail}`)
    }
  }

  // A forced sign-in runs before the check, so what gets reported is the session
  // that now exists rather than the one that did a moment ago.
  if (options.force === true) {
    for (const provider of wanted) {
      log(`${provider.padEnd(8)}signing in: ${SIGN_IN_COMMANDS[provider]?.join(' ')}`)
      const attempt = signInWith(provider)
      if (!attempt.ok) log(`        ${attempt.detail}`)
    }
  }

  const context = buildLoginContext({ options, environment })
  const check = async (provider: string): Promise<ProviderStatus> => ({
    ...(await checks[provider]!(context)),
    required: declared[provider]!.required,
  })

  const statuses: ProviderStatus[] = []
  for (const provider of wanted) {
    let status = await check(provider)

    // A required provider that is not signed in is the one case worth
    // interrupting for: everything after it fails anyway, and the fix is one
    // command the operator has to complete at browser speed. Optional providers
    // are reported and stepped over, and without a terminal there is nobody to
    // ask — CI gets the report and the exit code and nothing else.
    if (status.state !== 'ready' && status.required && interactive && options.logout !== true) {
      const command = SIGN_IN_COMMANDS[provider]?.join(' ') ?? provider
      log(`${provider.padEnd(8)}${status.state}`)
      log(`        ${status.detail}`)
      if (await confirm(`        Run \`${command}\` now? [y/N] `)) {
        const attempt = signInWith(provider)
        if (!attempt.ok) log(`        ${attempt.detail}`)
        // Re-check rather than trusting the exit status: `gh auth status --json`
        // exits zero on a broken session, and a cancelled browser flow can too.
        status = await check(provider)
      }
    }
    statuses.push(status)
  }
  return report({ statuses, log })
}

const helpFor = (module: string): string => {
  const spec = MODULES[module]
  if (!spec) throw new UsageError(`Unknown module "${module}". Known modules: ${MODULE_NAMES.join(', ')}`)
  return moduleUsage(module, spec)
}

export const run = async ({
  argv,
  environment = process.env,
  cwd = process.cwd(),
  log = console.log,
  signInWith = signIn,
  signOutWith = signOut,
  confirm = askConfirm,
  interactive = isInteractive(),
  checks = LOGIN_CHECKS,
}: {
  argv: string[]
  environment?: NodeJS.ProcessEnv
  cwd?: string
  log?: Log
  signInWith?: (provider: string) => SignInResult
  signOutWith?: (provider: string) => SignInResult
  confirm?: Confirm
  interactive?: boolean
  checks?: Record<string, ProviderCheck>
}): Promise<number> => {
  if (argv.length === 0 || argv[0] === 'help' || argv[0] === '--help') {
    log(usage())
    return argv.length === 0 ? 1 : 0
  }

  const { module, command, options, positionals, inner } = parseInvocation(argv, environment)
  if (options.help === true) {
    log(helpFor(module))
    return 0
  }
  const spec = MODULES[module]
  if (!spec) throw new UsageError(`Unknown module "${module}". Known modules: ${MODULE_NAMES.join(', ')}`)
  if (spec.scope === 'login') {
    return runLogin({
      command,
      options,
      environment,
      cwd,
      log,
      signInWith,
      signOutWith,
      confirm,
      interactive,
      checks,
    })
  }

  const commandSpec = spec.commands?.[command as string]
  if (!commandSpec) {
    const known = Object.keys(spec.commands ?? {}).join(', ')
    throw new UsageError(`Unknown command "${command ?? '<none>'}" for module ${module}. Known commands: ${known}`)
  }
  if (commandSpec.argument) {
    if (positionals.length === 0 && !commandSpec.argument.optional) {
      throw new UsageError(`mstage ${module} ${command} needs ${commandSpec.argument.form}`)
    }
  } else if (commandSpec.inner !== 'required' && inner) {
    throw new UsageError(`mstage ${module} ${command} takes no inner command`)
  }

  const context = await buildStageContext({ options, environment, cwd })
  if (!commandSpec.run) throw new UsageError(`mstage ${module} ${command} cannot be run directly`)
  const result = await commandSpec.run({ ...context, options, positionals, inner, log })
  return typeof result === 'number' ? result : 0
}
