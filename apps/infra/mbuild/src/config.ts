/*
 * Reads `mbuild.config.json`: how a repository is built, and where its
 * artifacts are uploaded.
 *
 * mbuild is shared, like mstage. Nothing in this module knows what a backoffice
 * or a commerce is — a repository declares its own artifacts and its own
 * registry here, and the same code publishes any of them. Three files now, and
 * the split between them is by *when* a value is decided rather than by what it
 * describes:
 *
 *   mstage.config.json     which stages exist, and what the store may hand out
 *   mbuild.config.json   what is built, and where it is uploaded         ← here
 *   mdeploy.config.json  what shape the stage is deployed into
 *
 * A build happens on a push to main and knows nothing about which stage will
 * run it; a deploy happens later and names a commit. The registries live here
 * rather than in mdeploy's file so the two cannot drift: the publishing
 * workflow and the deploy both read this one.
 *
 * Registries are per stage because they differ per stage — a repository whose
 * name says which stage may pull from it. What is built is not: the same
 * Dockerfiles produce the same artifacts wherever they end up, and an artifact
 * that existed in one stage but not another would make a promotion mean
 * something different depending on where it landed.
 *
 * The region is deliberately absent. mstage already declares where each stage
 * lives, and a second copy here would be one more thing to keep in step for no
 * benefit — a caller resolves it from mstage and passes it in.
 *
 * The tag is not here. It is the commit being built, which is different every
 * time and therefore an argument.
 */

import { readFileSync } from 'node:fs'
import { dirname, isAbsolute, join, resolve } from 'node:path'

export const CONFIG_FILENAME = 'mbuild.config.json'

export class BuildConfigError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'BuildConfigError'
  }
}

/** Findings that fail a publish. Anything else is recorded and allowed. */
export type ScanSeverity = 'CRITICAL' | 'HIGH' | 'MEDIUM' | 'LOW' | 'INFORMATIONAL' | 'UNDEFINED'

/**
 * How one artifact is built.
 *
 * Both paths are relative to `repository`, not to this file and not to whatever
 * directory a caller happens to be in. `mbuild publish` run from apps/infra and
 * the same command run from a workflow have to build the same bytes.
 */
export type ArtifactConfig = {
  /** Repository-relative path to the Dockerfile. */
  dockerfile: string
  /** Build context, repository-relative. Usually the root, for a workspace. */
  context: string
}

/** Where one stage's artifacts are uploaded. The kind decides the address shape. */
export type RegistryConfig = {
  kind: 'ecr' | 'artifact-registry'
  /** One repository holds every artifact this repository publishes. */
  repository: string
  /** A tag that can be repointed under a running service is not a version. */
  immutableTags: boolean
  scanOnPush: boolean
}

export type StageConfig = {
  registry: RegistryConfig
}

export type BuildConfig = {
  path: string
  /** The directory this file is in. */
  root: string
  /**
   * The repository root, resolved. Every artifact path is relative to this
   * rather than to the config file, because the Dockerfiles belong to the
   * repository and this file happens to live one directory down from it.
   * Resolved once here so nothing downstream depends on a working directory.
   */
  repository: string
  /** The same everywhere: what is built does not depend on where it lands. */
  artifacts: Record<string, ArtifactConfig>
  scan: {
    blockOn: ScanSeverity[]
    /** How long to wait for a scan to report before giving up on it. */
    timeoutSeconds: number
  }
  stages: Record<string, StageConfig>
}

const REGISTRY_KINDS: RegistryConfig['kind'][] = ['ecr', 'artifact-registry']

const SEVERITIES: ScanSeverity[] = ['CRITICAL', 'HIGH', 'MEDIUM', 'LOW', 'INFORMATIONAL', 'UNDEFINED']

/**
 * ECR allows letters, digits and `_ - . /`, starting with a letter or digit.
 * Artifact Registry is stricter — letters, digits and `-` — so this takes the
 * intersection, which is what a name usable on both looks like.
 */
const REPOSITORY_NAME = /^[a-z0-9][a-z0-9-]{0,62}$/

/** An artifact name becomes part of an image name or tag, depending on the kind. */
const ARTIFACT_NAME = /^[a-z][a-z0-9-]{0,30}$/

/** The same names mstage declares, so one file's stage means the other's. */
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
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new BuildConfigError(`${where} must be an object`)
  }
  return value as Record<string, unknown>
}

const assertKeys = (block: Record<string, unknown>, known: string[], where: string): void => {
  const unknown = Object.keys(block).filter((key) => !known.includes(key))
  if (unknown.length > 0) {
    throw new BuildConfigError(`${where} does not take ${unknown.join(', ')}. It takes ${known.join(', ')}`)
  }
  const missing = known.filter((key) => !(key in block))
  if (missing.length > 0) throw new BuildConfigError(`${where} must set ${missing.join(', ')}`)
}

/** A path that stays inside the repository, which is the whole build context. */
const assertContainedPath = (value: unknown, where: string): string => {
  if (typeof value !== 'string' || value.trim() === '' || isAbsolute(value)) {
    throw new BuildConfigError(`${where} must be a path inside the repository`)
  }
  if (value.split('/').includes('..')) {
    throw new BuildConfigError(`${where} must not climb out of the repository`)
  }
  return value
}

const parseArtifacts = (raw: unknown, where: string): Record<string, ArtifactConfig> => {
  const block = assertObject(raw, where)
  const names = Object.keys(block)
  if (names.length === 0) throw new BuildConfigError(`${where} must declare at least one artifact`)
  const parsed: Record<string, ArtifactConfig> = {}
  for (const name of names) {
    if (!ARTIFACT_NAME.test(name)) {
      throw new BuildConfigError(`${where}: artifact "${name}" must match ${ARTIFACT_NAME.source}`)
    }
    const artifact = assertObject(block[name], `${where}.${name}`)
    assertKeys(artifact, ['dockerfile', 'context'], `${where}.${name}`)
    parsed[name] = {
      dockerfile: assertContainedPath(artifact.dockerfile, `${where}.${name}.dockerfile`),
      context: assertContainedPath(artifact.context, `${where}.${name}.context`),
    }
  }
  return parsed
}

const parseRegistry = (raw: unknown, where: string): RegistryConfig => {
  const block = assertObject(raw, where)
  assertKeys(block, ['kind', 'repository', 'immutableTags', 'scanOnPush'], where)
  if (!REGISTRY_KINDS.includes(block.kind as RegistryConfig['kind'])) {
    throw new BuildConfigError(`${where}.kind must be one of ${REGISTRY_KINDS.join(', ')}`)
  }
  const kind = block.kind as RegistryConfig['kind']
  if (typeof block.repository !== 'string' || !REPOSITORY_NAME.test(block.repository)) {
    throw new BuildConfigError(`${where}.repository must match ${REPOSITORY_NAME.source}`)
  }
  for (const flag of ['immutableTags', 'scanOnPush'] as const) {
    if (typeof block[flag] !== 'boolean') throw new BuildConfigError(`${where}.${flag} must be true or false`)
  }
  return {
    kind,
    repository: block.repository,
    immutableTags: block.immutableTags as boolean,
    scanOnPush: block.scanOnPush as boolean,
  }
}

const parseStages = (raw: unknown, where: string): Record<string, StageConfig> => {
  const block = assertObject(raw, where)
  const names = Object.keys(block)
  if (names.length === 0) throw new BuildConfigError(`${where} must declare at least one stage`)
  const parsed: Record<string, StageConfig> = {}
  for (const name of names) {
    if (!STAGE_NAME.test(name)) {
      throw new BuildConfigError(`${where}: stage "${name}" may only contain letters, digits and "-"`)
    }
    const stage = assertObject(block[name], `${where}.${name}`)
    assertKeys(stage, ['registry'], `${where}.${name}`)
    parsed[name] = { registry: parseRegistry(stage.registry, `${where}.${name}.registry`) }
  }
  return parsed
}

const parseScan = (raw: unknown, where: string): BuildConfig['scan'] => {
  const block = assertObject(raw, where)
  assertKeys(block, ['blockOn', 'timeoutSeconds'], where)
  if (!Array.isArray(block.blockOn) || block.blockOn.length === 0) {
    throw new BuildConfigError(`${where}.blockOn must be a non-empty array`)
  }
  for (const severity of block.blockOn) {
    if (!SEVERITIES.includes(severity as ScanSeverity)) {
      throw new BuildConfigError(`${where}.blockOn must contain only ${SEVERITIES.join(', ')}`)
    }
  }
  const timeout = block.timeoutSeconds
  if (typeof timeout !== 'number' || !Number.isInteger(timeout) || timeout < 1 || timeout > 3_600) {
    throw new BuildConfigError(`${where}.timeoutSeconds must be a whole number from 1 to 3600`)
  }
  return { blockOn: block.blockOn as ScanSeverity[], timeoutSeconds: timeout }
}

export const parseBuildConfig = (path: string, contents: string): BuildConfig => {
  let raw: unknown
  try {
    raw = JSON.parse(contents)
  } catch (error) {
    throw new BuildConfigError(`${path} is not valid JSON: ${(error as Error).message}`)
  }
  const root = assertObject(raw, path)
  assertKeys(root, ['root', 'artifacts', 'scan', 'stages'], path)
  if (typeof root.root !== 'string' || root.root.trim() === '') {
    throw new BuildConfigError(`${path}: "root" must name the repository root, relative to this file`)
  }
  return {
    path,
    root: dirname(path),
    repository: resolve(dirname(path), root.root),
    artifacts: parseArtifacts(root.artifacts, `${path}: "artifacts"`),
    scan: parseScan(root.scan, `${path}: "scan"`),
    stages: parseStages(root.stages, `${path}: "stages"`),
  }
}

export const loadBuildConfig = ({
  cwd = process.cwd(),
  environment = process.env,
}: { cwd?: string; environment?: NodeJS.ProcessEnv } = {}): BuildConfig => {
  const override = environment.MBUILD_CONFIG
  const path = override ? (isAbsolute(override) ? override : resolve(cwd, override)) : findUp(cwd, CONFIG_FILENAME)
  if (!path) throw new BuildConfigError(`Could not find ${CONFIG_FILENAME} in ${cwd} or any parent directory`)
  let contents: string
  try {
    contents = readFileSync(path, 'utf8')
  } catch (error) {
    throw new BuildConfigError(`Could not read ${path}: ${(error as Error).message}`)
  }
  return parseBuildConfig(path, contents)
}

/**
 * One stage's registry. A stage this file does not declare is a typo rather
 * than a new environment — publishing into an undeclared repository would
 * create it, and nothing would ever pull from it.
 */
export const registryFor = (config: BuildConfig, stage: string): RegistryConfig => {
  const declared = config.stages[stage]
  if (!declared) {
    throw new BuildConfigError(
      `${config.path} declares no stage "${stage}". Declared: ${Object.keys(config.stages).join(', ')}`,
    )
  }
  return declared.registry
}
