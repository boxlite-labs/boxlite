/**
 * Whether the GitHub and Auth0 CLIs are signed in.
 *
 * Same rule as AWS: mstage signs nobody in and rewrites nobody's error. You run
 * `gh auth login` or `auth0 login`; mstage reads the session those left behind and
 * reports what the tool itself said when there isn't one.
 */

import { spawnSync } from 'node:child_process'

export type SignInResult = { ok: boolean; detail?: string }
export type ProviderTool = {
  signIn: string[]
  signOut: string[]
  install: { manager: string; formula: string }
  machineSignIn?: (credentials: MachineCredentials) => string[]
}
export type MachineCredentials = { domain: string; clientId: string; clientSecret: string }
type RunCommand = (command: string, args: string[], options: any) => any

const CLI_TIMEOUT_MS = 15_000

/**
 * The CLI each provider signs in through, and how to install it when missing.
 *
 * Sign-ins are interactive — a browser opens, or the CLI prompts — so they run
 * only when `--force` asks for one, and they inherit this terminal. Which of
 * these a repository actually needs is not decided here; mstage.config.json says.
 */
export const PROVIDER_TOOLS: Record<string, ProviderTool> = {
  // `aws login` (CLI 2.32+) trades a browser console session for short-lived
  // credentials. Note the formula name differs from the binary name.
  aws: { signIn: ['aws', 'login'], signOut: ['aws', 'logout'], install: { manager: 'brew', formula: 'awscli' } },
  // Application default credentials, not the user session: a deploy runs as ADC,
  // and signing a person in without them leaves every SDK call unauthenticated.
  gcp: {
    signIn: ['gcloud', 'auth', 'application-default', 'login'],
    signOut: ['gcloud', 'auth', 'application-default', 'revoke'],
    // Homebrew renamed this cask from google-cloud-sdk; the old name is still an
    // alias for it, and an alias is not what a hint should teach.
    install: { manager: 'brew', formula: 'gcloud-cli' },
  },
  github: {
    signIn: ['gh', 'auth', 'login'],
    // Prompts when more than one account is known; that choice is the operator's.
    signOut: ['gh', 'auth', 'logout'],
    install: { manager: 'brew', formula: 'gh' },
  },
  auth0: {
    signIn: ['auth0', 'login'],
    // Bare form logs out the active tenant; a tenant argument targets another.
    signOut: ['auth0', 'logout'],
    install: { manager: 'brew', formula: 'auth0' },
    // `auth0 login --help`: "Authenticates the Auth0 CLI using either personal
    // credentials (user login) or client credentials (machine login). Use
    // machine login for servers, CI, or any non-interactive environments."
    machineSignIn: ({ domain, clientId, clientSecret }) => [
      'auth0',
      'login',
      '--domain',
      domain,
      '--client-id',
      clientId,
      '--client-secret',
      clientSecret,
      '--no-input',
      '--no-color',
    ],
  },
}

export const SIGN_IN_COMMANDS = Object.fromEntries(
  Object.entries(PROVIDER_TOOLS).map(([key, tool]) => [key, tool.signIn]),
)

export const SIGN_OUT_COMMANDS = Object.fromEntries(
  Object.entries(PROVIDER_TOOLS).map(([key, tool]) => [key, tool.signOut]),
)

const INSTALL_MANAGERS: Record<string, (formula: string) => string> = {
  brew: (formula: string) => `brew install ${formula}`,
}

/** @returns {string} the sentence to append when a provider's CLI is absent */
export const installHint = (provider: string): string => {
  const install = PROVIDER_TOOLS[provider]?.install
  const manager = install && INSTALL_MANAGERS[install.manager]
  return manager ? ` Install it with: ${manager(install.formula)}` : ''
}

/** @returns {{ ok: boolean, detail?: string }} */
const runInteractive = (
  provider: string,
  argv: string[] | undefined,
  runCommand: RunCommand,
  what: string,
): SignInResult => {
  if (!argv) return { ok: false, detail: `mstage has no ${what} command for ${provider}` }
  const [command, ...args] = argv as string[]
  const result = runCommand(command!, args, { stdio: 'inherit' })
  if (result.error) {
    const missing = result.error.code === 'ENOENT'
    return {
      ok: false,
      detail: missing ? `${command} is not installed.${installHint(provider)}` : result.error.message,
    }
  }
  if (result.status !== 0) return { ok: false, detail: `${argv.join(' ')} exited with ${result.status}` }
  return { ok: true }
}

export const signIn = (provider: string, runCommand: RunCommand = spawnSync as RunCommand): SignInResult =>
  runInteractive(provider, SIGN_IN_COMMANDS[provider], runCommand, 'sign-in')

/**
 * Ends the session mstage reads. Interactive for the same reason a sign-in is:
 * `gh auth logout` asks which account when it knows more than one, and that
 * choice belongs to the operator rather than to a default mstage invents.
 */
export const signOut = (provider: string, runCommand: RunCommand = spawnSync as RunCommand): SignInResult =>
  runInteractive(provider, SIGN_OUT_COMMANDS[provider], runCommand, 'sign-out')

/**
 * Sign in as an application rather than as a person.
 *
 * Where the credentials come from is deliberately the caller's problem: on this
 * platform they live in the stage's secret store, which cannot be read until AWS
 * credentials exist, so the read has to happen after `login aws` succeeds. This
 * function only performs the exchange it is handed.
 *
 * The resulting session replaces whatever session the CLI held for that tenant,
 * which on a personal machine means the operator's interactive session is gone
 * until they sign in again.
 *
 * @param {'auth0'} provider
 * @param {{ domain: string, clientId: string, clientSecret: string }} credentials
 * @returns {{ ok: boolean, detail?: string }}
 */
export const signInWithClientCredentials = (
  provider: string,
  credentials: MachineCredentials,
  runCommand: RunCommand = spawnSync as RunCommand,
): SignInResult => {
  const build = PROVIDER_TOOLS[provider]?.machineSignIn
  if (!build) {
    const supported = Object.keys(PROVIDER_TOOLS).filter((key) => PROVIDER_TOOLS[key].machineSignIn)
    return { ok: false, detail: `${provider} has no machine login. Supported: ${supported.join(', ') || 'none'}` }
  }
  for (const field of ['domain', 'clientId', 'clientSecret'] as const) {
    // Never name the value, only the field: this runs with a live secret.
    if (!credentials?.[field]) return { ok: false, detail: `machine login needs a non-empty ${field}` }
  }

  const [command, ...args] = build(credentials)
  // stdio is piped, not inherited: a machine login has nothing to prompt for,
  // and its output can quote back what it was given.
  const result = runCommand(command, args, { encoding: 'utf8', timeout: CLI_TIMEOUT_MS })
  if (result.error) {
    const missing = result.error.code === 'ENOENT'
    return {
      ok: false,
      detail: missing ? `${command} is not installed.${installHint(provider)}` : result.error.message,
    }
  }
  if (result.status !== 0) {
    return { ok: false, detail: redact(result.stderr || result.stdout || `exited with ${result.status}`, credentials) }
  }
  return { ok: true }
}

/** The CLI echoes its arguments on some failures; the secret must not reach a log. */
const redact = (text: unknown, { clientSecret }: MachineCredentials): string =>
  String(text).split(clientSecret).join('***').trim().slice(0, 400)

const capture = (
  provider: string,
  command: string,
  args: string[],
  runCommand: RunCommand = spawnSync as RunCommand,
) => {
  const result = runCommand(command, args, { encoding: 'utf8', timeout: CLI_TIMEOUT_MS })
  if (result.error) {
    const missing = result.error.code === 'ENOENT'
    return {
      ok: false,
      detail: missing ? `${command} is not installed.${installHint(provider)}` : result.error.message,
    }
  }
  if (result.signal) return { ok: false, detail: `${command} did not finish (${result.signal})` }
  return {
    ok: result.status === 0,
    stdout: result.stdout ?? '',
    detail: (result.stderr || result.stdout || `${command} exited with ${result.status}`).trim(),
  }
}

const parse = (text: string, command: string): { value?: any; failure?: string } => {
  try {
    return { value: JSON.parse(text) }
  } catch {
    return { failure: `could not read ${command} output as JSON` }
  }
}

const notSignedIn = (provider: string, detail: string) => ({
  provider,
  state: 'not signed in',
  detail,
  expiresAt: null,
})

/**
 * `gh auth status --json` always exits zero unless the CLI itself fails, so the
 * per-host `state` is what says whether the session works.
 */
/**
 * Whether Application Default Credentials can actually authenticate.
 *
 * ADC rather than the logged-in user: a deploy runs as ADC, so checking
 * `gcloud auth list` would report a person being signed in while the thing that
 * actually authenticates is absent. On a runner there is no user at all and ADC
 * comes from workload identity, which this still sees.
 *
 * Which project a stage lives in is deliberately not checked. `mstage.config.json`
 * declares it per stage, `home.ts` hands that to every client explicitly, and the
 * gcloud calls that act on a project — `iam/src/gcp.ts`, mbuild's Artifact
 * Registry registrar — pass `--project` themselves. So gating on
 * `gcloud config get-value project` reported usable credentials as "not signed
 * in" over a setting the work does not consult.
 *
 * One call does consult it, and it is the reason a gate here looks tempting:
 * `gcloud auth application-default login` writes the configured project into ADC
 * as its `quota_project_id`. But that is written at login time, so a project set
 * afterwards satisfies such a gate and changes nothing about the credential
 * already on disk — the check would pass and the hazard would remain. Catching
 * that means reading the quota project out of ADC, which no gcloud command
 * reports, not asking whether some global is populated.
 *
 * `checkAws` is the mirror — it proves the credential resolves and leaves the
 * region to the stage.
 */
export const checkGcp = async ({ runCommand }: { runCommand?: RunCommand } = {} as any) => {
  const advice = 'Sign in with: gcloud auth application-default login'
  const result = capture('gcp', 'gcloud', ['auth', 'application-default', 'print-access-token'], runCommand)
  if (!result.ok) return notSignedIn('gcp', `${result.detail}. ${advice}`)
  return {
    provider: 'gcp',
    state: 'ready',
    // No account name: ADC carries no identity gcloud will report, and the one
    // thing this proved is that a token can be minted.
    detail: 'application default credentials are usable',
    // That token expires, but ADC mints another on demand; there is no deadline
    // a caller could act on.
    expiresAt: null,
  }
}

export const checkGitHub = async ({ runCommand }: { runCommand?: RunCommand } = {} as any) => {
  const result = capture('github', 'gh', ['auth', 'status', '--active', '--json', 'hosts'], runCommand)
  if (!result.ok) return notSignedIn('github', `${result.detail}. Sign in with: gh auth login`)

  const parsed = parse(result.stdout, 'gh')
  if (parsed.failure) return notSignedIn('github', parsed.failure)

  const accounts = Object.values(parsed.value?.hosts ?? {}).flat() as any[]
  const active = accounts.find((account: any) => account.active)
  if (!active) return notSignedIn('github', 'no active account. Sign in with: gh auth login')
  if (active.state !== 'success') {
    return notSignedIn('github', `${active.login ?? 'account'} on ${active.host}: ${active.state}`)
  }
  return {
    provider: 'github',
    state: 'ready',
    detail: `${active.login} on ${active.host}`,
    expiresAt: null,
  }
}

export const checkAuth0 = async ({ runCommand }: { runCommand?: RunCommand } = {} as any) => {
  const result = capture('auth0', 'auth0', ['tenants', 'list', '--json', '--no-input', '--no-color'], runCommand)
  if (!result.ok) return notSignedIn('auth0', `${result.detail}. Sign in with: auth0 login`)

  const parsed = parse(result.stdout, 'auth0')
  if (parsed.failure) return notSignedIn('auth0', parsed.failure)

  const tenants = Array.isArray(parsed.value) ? parsed.value : []
  const active = tenants.find((tenant: any) => tenant.active)
  if (!active) return notSignedIn('auth0', 'no active tenant. Sign in with: auth0 login')
  return { provider: 'auth0', state: 'ready', detail: active.name, expiresAt: null }
}
