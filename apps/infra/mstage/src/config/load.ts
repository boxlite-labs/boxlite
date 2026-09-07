/**
 * Reads `mstage.config.json`, the per-repository description of what mstage may touch.
 *
 * The file is the reason a stage name means something: it names the stages that
 * exist and the region each one lives in. A stage that is not declared here is a
 * typo, not a new environment, and mstage refuses it rather than opening a fresh
 * secret namespace under the misspelled name.
 *
 * Which tenant a stage lives in is deliberately absent on AWS: the account is
 * whichever one the resolved credentials belong to, read back from STS by the
 * tools that need to name it in an ARN. GCP is different — its clients cannot be
 * built without a project — so a GCP stage declares `project` and nothing else
 * does.
 *
 * It also names which sign-ins the repository needs. mstage itself is shared by
 * several repositories and has no opinion about that: boxlite-commerce needs AWS
 * alone, while a console that dispatches GitHub deployments needs more.
 *
 * How a stage is deployed is deliberately absent — that belongs to the
 * repository's own deploy tool, which calls mstage rather than living inside it.
 */

import { readFileSync } from 'node:fs'
import { dirname, isAbsolute, join, resolve } from 'node:path'
import { SECRET_GROUP } from '../env/secret-address.ts'

export const CONFIG_FILENAME = 'mstage.config.json'

export class ConfigError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ConfigError'
  }
}

/** Which cloud this repository lives in. One value for every stage in it. */
export type Cloud = 'aws' | 'gcp'

export type StageConfig = {
  region: string | null
  /**
   * Which cloud this one stage lives in, or null to take the repository's.
   *
   * The root `home` is the default and answers for every stage that says
   * nothing, which is the shape a repository living in one cloud has. BoxLite
   * is not that shape: its AWS stages are in service while a GCP stage is being
   * brought up beside them, and the two have to be deployable from one checkout
   * rather than by editing one field back and forth. Overriding it per stage is
   * what makes a stage — not a branch of the config file — the unit that moves.
   */
  home: Cloud | null
  /** The GCP project this stage lives in. Null on AWS, which reads it from STS. */
  project: string | null
  roleArn: string | null
  protect: boolean
}

export type LoginRequirement = { required: boolean }

/**
 * Named subsets of the store that may leave it.
 *
 * The store is the one place configuration lives, so everything that needs some
 * of it — a deploy, a server — asks for a group by name rather than for the
 * whole thing. Adding a key to a group is a reviewable edit to this file, which
 * is the only reason exporting is safe at all.
 *
 * One group name means more than the rest: `secret` says its keys hold the
 * *address* of a secret rather than the secret, which is what lets a workload be
 * handed one by reference (`env/secret-address.ts`).
 *
 * Every key a group names, required and optional together. What tells the two
 * apart is `envOptional` below.
 */
export type EnvExports = Record<string, string[]>

/**
 * A group may be written two ways.
 *
 * An array is the short form and means every key is required. The object form
 * separates the two, and exists because "the store must hold this" and "this
 * stage may not have set this" are different statements that a single list
 * cannot make.
 *
 * The distinction is not cosmetic. A missing *required* key is the failure
 * `valuesOfGroup` refuses on purpose: a process handed a silently short
 * environment fails hours later, somewhere that does not mention the key. A
 * missing *optional* key is a feature this stage did not configure — a billing
 * origin nobody set, an incident.io source nobody wired — and the consumer
 * already has an answer for it. Forcing those into the required list means
 * seeding a row of empty strings per stage to say nothing at all.
 */
export type EnvGroupDeclaration = string[] | { required?: string[]; optional?: string[] }

/**
 * Which key holds the fingerprint of which group. Absent means this repository
 * does not fingerprint its configuration, and `--digest` has nothing to write.
 */
export type EnvDigest = { key: string; group: string }

export type MstageConfig = {
  path: string
  root: string
  app: string
  home: Cloud
  login: Record<string, LoginRequirement>
  /** Every key each group names, required and optional together. */
  envSelectGroup: EnvExports
  /**
   * The subset of each group whose keys the store need not hold.
   *
   * A separate map rather than a richer `envSelectGroup`, because almost every
   * consumer asks only "which keys does this group name" — the CLI's listing,
   * the secret-address check, the digest's own key set. Splitting the type
   * there would have made all of them ask a question they do not have.
   */
  envOptional: EnvExports
  envDigest: EnvDigest | null
  stages: Record<string, StageConfig>
}

const STAGE_NAME = /^[a-zA-Z0-9-]+$/

const findUp = (from: string, filename: string): string | null => {
  let directory = resolve(from)
  for (;;) {
    const candidate = join(directory, filename)
    try {
      readFileSync(candidate)
      return candidate
    } catch {
      const parent = dirname(directory)
      if (parent === directory) return null
      directory = parent
    }
  }
}

const assertObject = (value: unknown, where: string): Record<string, unknown> => {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new ConfigError(`${where} must be an object`)
  return value as Record<string, unknown>
}

const assertNonEmptyString = (value: unknown, where: string): string => {
  if (typeof value !== 'string' || value.trim() === '') throw new ConfigError(`${where} must be a non-empty string`)
  return value
}

/** The same shape SST requires of a name it can set (cmd/sst/secret.go:363). */
const ENV_KEY = /^[A-Z][a-zA-Z0-9_]*$/

/**
 * A marked key belongs to some group that delivers it.
 *
 * `secret` names no consumer. It marks which keys hold the address of a secret
 * rather than the secret, and some other group — whatever this repository calls
 * the thing that reads it — decides who receives it. A key marked and delivered
 * nowhere is a mark on nothing: the store holds an address no consumer asks for,
 * and the mistake is invisible until someone wonders why the secret never
 * arrived.
 */
const assertMarkedKeysAreDelivered = (groups: EnvExports, path: string): void => {
  const marked = groups[SECRET_GROUP]
  if (!marked) return
  const delivered = new Set(
    Object.entries(groups)
      .filter(([group]) => group !== SECRET_GROUP)
      .flatMap(([, keys]) => keys),
  )
  const orphans = marked.filter((key) => !delivered.has(key))
  if (orphans.length > 0) {
    throw new ConfigError(
      `${path}: "env.selectGroup.${SECRET_GROUP}" marks ${orphans.join(', ')}, which no other group names; ` +
        'the mark says a key holds an address, and some group has to say who reads it',
    )
  }
}

/** One list of key names, checked for shape and for repeats. */
const parseKeyList = (keys: unknown, where: string): string[] => {
  // Empty is a state, not a mistake. A group names a set a consumer asks for,
  // and a consumer that has one service per group needs to say "this service
  // reads nothing yet" — refusing that forces a placeholder key or no
  // declaration at all, and the second hides the service entirely.
  if (!Array.isArray(keys)) throw new ConfigError(`${where} must be an array of key names`)
  for (const key of keys) {
    if (typeof key !== 'string' || !ENV_KEY.test(key)) {
      throw new ConfigError(`${where} contains ${JSON.stringify(key)}, which is not a key name`)
    }
  }
  const duplicates = (keys as string[]).filter((key, index) => keys.indexOf(key) !== index)
  if (duplicates.length > 0) {
    throw new ConfigError(`${where} repeats ${[...new Set(duplicates)].join(', ')}`)
  }
  return [...(keys as string[])]
}

/**
 * Both forms of a group: the bare array, and the required/optional object.
 *
 * Returns the union and the optional half separately, because those are the two
 * questions consumers ask and neither is derivable from the other.
 */
const parseEnvSelectGroup = (raw: unknown, path: string): { all: EnvExports; optional: EnvExports } => {
  if (raw === undefined) return { all: {}, optional: {} }
  const env = assertObject(raw, `${path}: "env"`)
  if (env.selectGroup === undefined) return { all: {}, optional: {} }
  const show = assertObject(env.selectGroup, `${path}: "env.selectGroup"`)

  const all: EnvExports = {}
  const optional: EnvExports = {}
  for (const [group, declared] of Object.entries(show)) {
    const where = `${path}: "env.selectGroup.${group}"`
    if (Array.isArray(declared)) {
      all[group] = parseKeyList(declared, where)
      optional[group] = []
      continue
    }
    if (!declared || typeof declared !== 'object') {
      // Named here rather than left to `assertObject`, so the refusal describes
      // both accepted shapes instead of only the one it happened to try last.
      throw new ConfigError(`${where} must be an array of key names, or an object with required and optional`)
    }
    const block = assertObject(declared, where)
    const unknown = Object.keys(block).filter((key) => key !== 'required' && key !== 'optional')
    if (unknown.length > 0) {
      throw new ConfigError(`${where} does not take ${unknown.join(', ')}. It takes required, optional`)
    }
    const required = parseKeyList(block.required ?? [], `${where}.required`)
    const mayBeAbsent = parseKeyList(block.optional ?? [], `${where}.optional`)
    const both = required.filter((key) => mayBeAbsent.includes(key))
    if (both.length > 0) {
      // A key cannot be two things. Left in, the required list would win and the
      // optional list would read as a promise the store never made.
      throw new ConfigError(`${where} names ${both.join(', ')} as both required and optional`)
    }
    all[group] = [...required, ...mayBeAbsent]
    optional[group] = mayBeAbsent
  }
  assertMarkedKeysAreDelivered(all, path)
  return { all, optional }
}

const parseEnvDigest = (
  raw: unknown,
  groups: EnvExports,
  optional: EnvExports,
  path: string,
): EnvDigest | null => {
  if (raw === undefined) return null
  const env = assertObject(raw, `${path}: "env"`)
  if (env.digest === undefined) return null
  const digest = assertObject(env.digest, `${path}: "env.digest"`)
  const key = assertNonEmptyString(digest.key, `${path}: "env.digest.key"`)
  if (!ENV_KEY.test(key)) throw new ConfigError(`${path}: "env.digest.key" ${JSON.stringify(key)} is not a key name`)
  const group = (digest.group as string) ?? 'deploy'
  if (!groups[group]) {
    const known = Object.keys(groups).join(', ') || '(none)'
    throw new ConfigError(`${path}: "env.digest.group" names "${group}", which env.selectGroup does not declare: ${known}`)
  }
  if (!groups[group].includes(key)) {
    // The digest travels with the group it describes, so a consumer that reads
    // the group has it without a second lookup.
    throw new ConfigError(`${path}: env.selectGroup.${group} must include ${key}, the key its digest is written to`)
  }
  if ((optional[group] ?? []).includes(key)) {
    // An optional fingerprint is no fingerprint: the check that compares it
    // would pass on every stage that simply never wrote one.
    throw new ConfigError(`${path}: ${key} is the digest of env.selectGroup.${group} and cannot be optional`)
  }
  return { key, group }
}

const parseStage = (name: string, raw: unknown, path: string, repositoryHome: Cloud): StageConfig => {
  // SST's own constraint (pkg/project/project.go:115). mstage reads and writes the
  // same S3 keys, so a name SST would reject must never reach the bucket.
  if (!STAGE_NAME.test(name)) {
    throw new ConfigError(`${path}: stage "${name}" may only contain letters, digits and "-"`)
  }
  const stage = assertObject(raw, `${path}: stage "${name}"`)
  for (const key of ['region', 'project', 'roleArn'] as const) {
    if (stage[key] !== undefined) assertNonEmptyString(stage[key], `${path}: stage "${name}" ${key}`)
  }
  if (stage.protect !== undefined && typeof stage.protect !== 'boolean') {
    throw new ConfigError(`${path}: stage "${name}" protect must be true or false`)
  }
  if (stage.home !== undefined && stage.home !== 'aws' && stage.home !== 'gcp') {
    throw new ConfigError(`${path}: stage "${name}" home must be "aws" or "gcp"`)
  }
  const home = (stage.home as Cloud) ?? repositoryHome
  /*
   * Refused here rather than where the clients are built. A GCP stage's Storage
   * and Secret Manager clients cannot exist without a project, so the failure is
   * certain — and a config file is the only thing that can supply one, which
   * makes reading the file the right moment to say so.
   */
  if (home === 'gcp' && stage.project === undefined) {
    throw new ConfigError(
      `${path}: stage "${name}" lives in gcp and must declare a project; ` +
        'a GCP client cannot be built without one',
    )
  }
  return {
    region: (stage.region as string) ?? null,
    home: (stage.home as Cloud) ?? null,
    project: (stage.project as string) ?? null,
    roleArn: (stage.roleArn as string) ?? null,
    protect: (stage.protect as boolean) ?? false,
  }
}

const parseLogin = (raw: unknown, path: string): Record<string, LoginRequirement> => {
  if (raw === undefined) return {}
  const login = assertObject(raw, `${path}: "login"`)
  return Object.fromEntries(
    Object.entries(login).map(([provider, value]) => {
      const entry = assertObject(value, `${path}: login "${provider}"`)
      if (entry.required !== undefined && typeof entry.required !== 'boolean') {
        throw new ConfigError(`${path}: login "${provider}" required must be true or false`)
      }
      return [provider, { required: (entry.required as boolean) ?? true }]
    }),
  )
}

export const parseConfig = (path: string, contents: string): MstageConfig => {
  let raw: unknown
  try {
    raw = JSON.parse(contents)
  } catch (error) {
    throw new ConfigError(`${path} is not valid JSON: ${(error as Error).message}`)
  }
  const root = assertObject(raw, path)
  assertNonEmptyString(root.app, `${path}: "app"`)
  if (root.home !== 'aws' && root.home !== 'gcp') {
    throw new ConfigError(`${path}: "home" must be "aws" or "gcp"; mstage keeps a stage's configuration in one of those`)
  }
  // Bound once: narrowing a property of a `Record<string, unknown>` does not
  // survive into the closure each stage is parsed in.
  const home: Cloud = root.home
  const { all: envSelectGroup, optional: envOptional } = parseEnvSelectGroup(root.env, path)
  const stages = assertObject(root.stages, `${path}: "stages"`)
  const names = Object.keys(stages)
  if (names.length === 0) throw new ConfigError(`${path}: "stages" must declare at least one stage`)
  return {
    path,
    root: dirname(path),
    app: root.app as string,
    home,
    login: parseLogin(root.login, path),
    envSelectGroup,
    envOptional,
    envDigest: parseEnvDigest(root.env, envSelectGroup, envOptional, path),
    stages: Object.fromEntries(names.map((name) => [name, parseStage(name, stages[name], path, home)])),
  }
}

/**
 * Which cloud one stage lives in: its own declaration, or the repository's.
 *
 * The single place that answer is composed. Everything downstream — the store
 * backend, the identity, the provider bundle, the engine, the registry kind —
 * follows from it, so a second expression of the same fallback is a second
 * place the two could disagree.
 */
export const homeFor = (config: Pick<MstageConfig, 'home' | 'stages' | 'path'>, stage: string | null): Cloud => {
  if (!stage) return config.home
  const declared = config.stages[stage]
  if (!declared) {
    const known = Object.keys(config.stages).join(', ')
    throw new ConfigError(`${config.path} declares no stage "${stage}". Declared: ${known}`)
  }
  return declared.home ?? config.home
}

export const loadConfig = ({
  cwd = process.cwd(),
  environment = process.env,
}: { cwd?: string; environment?: NodeJS.ProcessEnv } = {}): MstageConfig => {
  const override = environment.MSTAGE_CONFIG
  const path = override ? (isAbsolute(override) ? override : resolve(cwd, override)) : findUp(cwd, CONFIG_FILENAME)
  if (!path) throw new ConfigError(`Could not find ${CONFIG_FILENAME} in ${cwd} or any parent directory`)
  let contents: string
  try {
    contents = readFileSync(path, 'utf8')
  } catch (error) {
    throw new ConfigError(`Could not read ${path}: ${(error as Error).message}`)
  }
  return parseConfig(path, contents)
}
