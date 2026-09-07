// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

/*
 * Helpers for bootstrap.ts — the idempotent, human-run environment preparation script.
 *
 * Nothing here calls AWS, GitHub or Auth0, so the naming, ARN construction, argv contract and
 * idempotency decisions are covered by unit tests instead of only being exercised against live
 * accounts. The one exception is withStageConfigFile, which writes the stage configuration to a
 * short-lived 0600 file because `sst secret load` reads a path and has no stdin form; it is here
 * rather than in bootstrap.ts so that file's three security properties stay testable.
 */

import { existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { parseArgs } from 'node:util'

import { parse } from 'dotenv'

import {
  STAGE_CONFIG_DIGEST_KEY,
  STAGE_CONFIG_MANIFEST_KEY,
  parseStageConfigManifest,
  serializeStageConfigManifest,
  stageConfigDigest,
} from '../deployment/stage-config.js'
import { isLocalOnlyDeploymentKey, isStorableStageConfigKey } from '../deployment/key-policy.js'

// No underscore: the stage is interpolated into the Runner artifacts bucket name
// (deployment/environment.ts's runnerArtifactsBucketName), and S3 bucket names may
// not contain one. Allowing `dev_blue` here would pass validation and then fail at
// `s3api create-bucket`, after bootstrap had already made external changes.
const STAGE_LIKE_PATTERN = /^[A-Za-z0-9][A-Za-z0-9-]*$/
const GITHUB_REPO_PATTERN = /^[A-Za-z0-9][A-Za-z0-9_.-]*\/[A-Za-z0-9][A-Za-z0-9_.-]*$/

// `aws login` (browser-based AWS Management Console credentials) is what lets a
// self-hoster bootstrap without minting long-lived access keys. It shipped in
// AWS CLI 2.32.0, so an older CLI silently lacks the whole flow and the
// operator would be sent to the IAM console instead.
export const MINIMUM_AWS_CLI_VERSION = '2.32.0'
export const GITHUB_OIDC_PROVIDER_URL = 'https://token.actions.githubusercontent.com'
const AWS_CLI_VERSION_PATTERN = /^aws-cli\/(\d+)\.(\d+)\.(\d+)/

function requireStageLike(name: any, value: any) {
  if (!value || !STAGE_LIKE_PATTERN.test(value)) {
    throw new Error(`${name} '${value}' must match ${STAGE_LIKE_PATTERN}`)
  }
  return value
}

/*
 * bootstrap's own flags.
 *
 * Strict on purpose. Non-strict parsing accepts `--repo --force` as repo="--force" and silently
 * drops the flag that followed, and it lets a boolean take an inline value — so `--provision-auth0=false`
 * would read as truthy and run the one step in this script that is not idempotent. Strict refuses
 * both, before anything external has been created.
 *
 * `--stage` is declared here only so strict parsing accepts it; its value is read by resolveSstStage,
 * which owns the stage grammar and the "may be specified only once" rule.
 */
export function parseBootstrapOptions(args: readonly string[]) {
  const { values } = parseArgs({
    args: [...args],
    strict: true,
    options: {
      confirm: { type: 'boolean' },
      force: { type: 'boolean' },
      'provision-auth0': { type: 'boolean' },
      'provision-ses': { type: 'boolean' },
      repo: { type: 'string' },
      reviewers: { type: 'string' },
      stage: { type: 'string' },
    },
  })
  // Spread off parseArgs' null-prototype object, so callers get something that behaves like every
  // other options object in this package.
  return { ...values }
}

export function validateGitHubRepo(repo: any) {
  if (!repo || !GITHUB_REPO_PATTERN.test(repo)) {
    throw new Error(`repo '${repo}' must look like 'owner/name'`)
  }
  return repo
}

export function ssmParameterName(stage: any, param: any) {
  requireStageLike('stage', stage)
  if (!param) throw new Error('param is required')
  return `/boxlite/${stage}/${param}`
}

/*
 * IAM role name for the GitHub deploy role.
 *
 * Called directly as `--role-name` when bootstrap/aws.ts's ensureDeployRole creates or updates the
 * role, and by the deploy workflows to compose its ARN from AWS_ACCOUNT_ID — one spelling used by
 * both, so a rename cannot drift between them the way two independent literals could.
 *
 * It sits outside the grammar awsResourceName composes (deployment/environment.ts) for now — see the
 * note there. Moving it costs an IAM role rename (AWS has no rename primitive, so this is a delete
 * and recreate under a session that trusted the old name) and an edit to every workflow that
 * composes the ARN, which is its own change rather than a rider on this one.
 */
export function githubDeployRoleName(stage: any) {
  requireStageLike('stage', stage)
  return `boxlite-${stage}-github-deploy`
}

// `aws --version` prints e.g. `aws-cli/2.35.11 Python/3.14.6 Darwin/27.0.0 source/arm64`.
export function parseAwsCliVersion(versionOutput: any) {
  const match = AWS_CLI_VERSION_PATTERN.exec((versionOutput ?? '').trim())
  if (!match) {
    throw new Error(`could not parse an AWS CLI version from '${versionOutput}' (expected 'aws-cli/X.Y.Z ...')`)
  }
  return { major: Number(match[1]), minor: Number(match[2]), patch: Number(match[3]) }
}

export function isAwsCliVersionAtLeast(versionOutput: any, minimum = MINIMUM_AWS_CLI_VERSION) {
  const actual = parseAwsCliVersion(versionOutput)
  const [major, minor, patch] = minimum.split('.').map(Number)
  const ordered = [
    [actual.major, major],
    [actual.minor, minor],
    [actual.patch, patch],
  ]
  for (const [left, right] of ordered) {
    if (left !== right) return left > right
  }
  return true
}

/*
 * IAM OIDC providers are account-global and `CreateOpenIDConnectProvider`
 * rejects a duplicate URL with EntityAlreadyExists — so a bootstrap that
 * unconditionally creates one breaks on every account that has ever wired
 * GitHub Actions to AWS. Detect first, create only when genuinely absent.
 */
export function hasGitHubOidcProvider(listOutput: any, url = GITHUB_OIDC_PROVIDER_URL) {
  const host = url.replace(/^https:\/\//, '')
  const providers = listOutput?.OpenIDConnectProviderList ?? []
  return providers.some(({ Arn }: any) => typeof Arn === 'string' && Arn.endsWith(`:oidc-provider/${host}`))
}

/*
 * Split the operator's local .env into the part that belongs in the stage's shared SST secret
 * store and the part that must stay on this machine.
 *
 * Two rules decide what is kept: the key must be one the deploy actually reads
 * (isStorableStageConfigKey, derived from source), and it must not be one that has to stay on this
 * machine (isLocalOnlyDeploymentKey — AWS_PROFILE, AWS_CLI_PATH, the artifact selectors CI owns, and
 * the keys consulted before the store can be reached at all). Anything else in .env is left here,
 * which is deliberate: an operator's file may hold all sorts of things, and only what a deploy reads
 * belongs in a store every deployer of the stage can read. They are kept OUT rather than made a hard error, because they
 * are legitimate locally — stack/app.ts reads AWS_PROFILE — so rejecting the file would leave an
 * operator who needs a named profile unable to bootstrap at all. Storing them would be worse than
 * either: a machine-specific `aws` path, or an artifact redirect, handed to every deployer of the
 * stage.
 *
 * Everything else is stored exactly as dotenv parsed it, empty values included. `KEY=` and an
 * absent key already mean the same thing to every consumer (envOr, requireEnv, and the
 * `process.env.X && {...}` spreads all treat '' as unset), so dropping blanks would add a rule
 * without changing a deploy.
 */
export function deployableStageConfig(source: any) {
  const parsed = parse(source ?? '')
  const config: Record<string, string> = {}
  const excluded = []
  for (const key of Object.keys(parsed).sort()) {
    if (!isStorableStageConfigKey(key) || isLocalOnlyDeploymentKey(key)) {
      excluded.push(key)
      continue
    }
    config[key] = parsed[key]
  }
  return { config, excluded }
}

/*
 * The stage config as a file `sst secret load` reads back byte-for-byte.
 *
 * sst's parser splits each line on the first `=`, trims both halves, and only then strips quotes.
 * The double-quoted form then runs an unescape pass over \" \n \r \t, so a value holding one of
 * those two-character sequences literally — `C:\new`, a token with a stray backslash — comes back
 * changed. The single-quoted form strips the quotes and does nothing else, so it round-trips, and
 * the quotes also protect a leading or trailing space from the trim and a `#` from being read as
 * the start of a comment.
 *
 * So single quotes are the default. A value holding one of its own cannot use them, and an apostrophe
 * is ordinary in a generated password — so that case takes the double-quoted form instead, but only
 * when the value contains no `"`, `\`, `$` or backtick.
 *
 * The `$` matters and is easy to get wrong: `sst secret load` is Go, and godotenv expands `$VAR`
 * inside double quotes. Checking this against JavaScript's dotenv says otherwise — that one does no
 * expansion — so a password containing `$` would round-trip in a unit test here and be stored as
 * something else by sst. The single-quoted form is exempt because neither parser expands inside it.
 *
 * What is left has no representation: a newline, which the one-assignment-per-line format cannot hold
 * at all, and a value mixing a single quote with one of those four characters. Both are refused here
 * rather than stored mangled, which would surface much later as an opaque auth failure. The value
 * itself stays out of the message — this runs over credentials.
 */
export function serializeStageConfig(config: any) {
  const lines = Object.keys(config ?? {})
    .sort()
    .map((key) => {
      const value = String(config[key])
      // A newline cannot be represented at all: the format is one assignment per line.
      if (/[\r\n]/.test(value)) {
        throw new Error(`${key} contains a newline, which the stage configuration store cannot represent`)
      }
      // Single quotes are the exact form — the parser strips them and does nothing else — so they are
      // the default whenever the value has none of its own.
      if (!value.includes("'")) return `${key}='${value}'`
      /*
       * An apostrophe is ordinary in a generated password or token, so refusing it outright would
       * block bootstrap over a perfectly valid secret. Double quotes carry it, but they also make the
       * parser process escapes — so this form is used only when there is nothing in the value for an
       * escape to alter. Anything else is refused rather than silently mangled into a different value.
       */
      if (!/["\\$`]/.test(value)) return `${key}="${value}"`
      throw new Error(
        `${key} mixes a single quote with one of " \\ $ or a backtick, which cannot be quoted for the ` +
          'stage configuration store without changing the value',
      )
    })
  return lines.length > 0 ? `${lines.join('\n')}\n` : ''
}

/*
 * The short-lived file `sst secret load` reads that serialized configuration from.
 *
 * `secret load` takes a path and has no stdin form, so the whole configuration — every domain, issuer
 * and token in the operator's .env — has to exist on disk for the length of one command. That is the
 * only reason this exists, and everything here is about keeping that window small and closed:
 *
 *   - a directory of its own, from mkdtemp, so the 0700 it creates keeps other users out even before
 *     the file inside it exists;
 *   - 0600 on the file as well, so a umask that would have widened it cannot;
 *   - removal in `finally`, so a failed load leaves nothing behind — which is the case that matters,
 *     since that is when someone is most likely to walk away from the terminal.
 */
export function withStageConfigFile<T>(config: any, use: (configPath: string) => T): T {
  const directory = mkdtempSync(join(tmpdir(), 'boxlite-stage-config-'))
  try {
    const configPath = join(directory, 'stage-config.env')
    writeFileSync(configPath, serializeStageConfig(config), { mode: 0o600 })
    return use(configPath)
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
}

/*
 * Whether sst's platform directory is usable. sst writes package.json on its
 * first run and only then installs the deps, so "package.json but no
 * node_modules" is the signature of an install that started and did not finish
 * — which is exactly what a stalled bun leaves behind.
 */
export function sstPlatformState(dir: any) {
  if (!existsSync(join(dir, 'package.json'))) return 'absent'
  try {
    return readdirSync(join(dir, 'node_modules')).length > 0 ? 'ready' : 'deps-missing'
  } catch {
    return 'deps-missing'
  }
}

/*
 * Everything one `sst secret load` will write, decided in one place.
 *
 * Both the values and the bookkeeping that describes them — the manifest of key names and the digest
 * hydration verifies against — because they have to go into the same load. Written apart, an
 * interruption between them leaves values whose digest describes a different generation, which is the
 * failure the digest exists to catch rather than cause.
 *
 * Pure, so bootstrap's .env-to-store wiring is checkable without an AWS account: the arguments, the
 * manifest and the refusals are all decided here.
 */
export function prepareStageConfigLoad(source: any) {
  const { config, excluded } = deployableStageConfig(source)
  const storedKeys = Object.keys(config)
  if (storedKeys.length === 0) {
    throw new Error(
      'defines no deployable stage configuration — every key is local-only or unread by the deploy',
    )
  }

  const manifest = serializeStageConfigManifest(storedKeys)
  // Typed as a plain map: the literal below would otherwise infer down to just the two bookkeeping
  // keys, and a caller reading a configuration value out of it would not typecheck.
  const payload: Record<string, string> = {
    ...config,
    [STAGE_CONFIG_MANIFEST_KEY]: manifest,
    // Over the manifest's own keys rather than over `config`, so the two sides hash the same list:
    // hydration has only the manifest to go by. A value that reaches the store without being named
    // is then outside the digest, which is correct — an unnamed key is never hydrated either.
    [STAGE_CONFIG_DIGEST_KEY]: stageConfigDigest(parseStageConfigManifest(manifest), config),
  }

  return { excluded, storedKeys, payload }
}
