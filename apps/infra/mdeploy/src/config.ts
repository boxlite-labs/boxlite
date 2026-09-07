/*
 * Reads `mdeploy.config.json`, the shape this repository deploys into.
 *
 * It sits beside `mstage.config.json` and deliberately holds the other half.
 * mstage's file says which stages exist, where they live and what the store may
 * hand out; this one says how big the database is, how long its backups are
 * kept, and whether a stage refuses deletion. A value belongs here when
 * changing it changes the infrastructure, and in mstage's file when changing it
 * changes what configuration a running thing reads.
 *
 * That line is worth stating for the two BoxLite values that look like they
 * could go either way. `STACK_DOMAIN` is in mstage's file: it is the hostname a
 * running API serves and a running dashboard calls, and moving a stage to
 * another domain changes no resource shape. `runners.size` is here: it decides
 * which machine family a host is created from, and on GCP it decides whether
 * nested virtualization is available at all.
 *
 * Neither file holds a secret, and neither holds anything a single deploy
 * decides — an image tag and a runner binary's checksum come from the
 * invocation, because they are different every time.
 *
 * A stage is written as an override of the defaults rather than as a complete
 * copy. Stages differ in a few deliberate ways and agree on everything else, and
 * a full copy per stage hides which of the differences were meant.
 */

import { readFileSync } from 'node:fs'
import { dirname, isAbsolute, join, resolve } from 'node:path'
import type { AlarmRequest, AlarmThreshold } from '../stack/alarms.ts'
import type { CacheRequest, CacheSize } from '../stack/cache.ts'
import type { ClickHouseMode, ClickHouseRequest } from '../stack/clickhouse.ts'
import type { DatabaseRequest, DatabaseSize } from '../stack/database.ts'
import type { RunnerRequest, RunnerSize } from '../stack/runners.ts'
import type { StorageRequest } from '../stack/storage.ts'

export const CONFIG_FILENAME = 'mdeploy.config.json'

export class DeployConfigError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'DeployConfigError'
  }
}

/** The runner settings a config file decides. The fleet and the binary do not. */
export type RunnerSettings = Pick<RunnerRequest, 'size' | 'rootDiskGb'>

/** What one stage overrides. Every section is optional, and so is every key in it. */
export type StageOverride = {
  database: Partial<DatabaseRequest>
  cache: Partial<CacheRequest>
  storage: Partial<StorageRequest>
  clickhouse: Partial<ClickHouseRequest>
  runners: Partial<RunnerSettings>
  alarms: Partial<AlarmRequest>
}

export type DeployConfig = {
  path: string
  root: string
  /** The defaults, before any stage has been named. */
  database: DatabaseRequest
  cache: CacheRequest
  storage: StorageRequest
  clickhouse: ClickHouseRequest
  runners: RunnerSettings
  alarms: AlarmRequest
  stages: Record<string, StageOverride>
}

const SECTIONS = ['database', 'cache', 'storage', 'clickhouse', 'runners', 'alarms'] as const

/** SST's own constraint on a stage name (pkg/project/project.go:115). */
const STAGE_NAME = /^[a-zA-Z0-9-]+$/

/** What PostgreSQL and ClickHouse both accept unquoted, which is the only form worth using. */
const UNQUOTED_NAME = /^[a-z][a-z0-9_]*$/

/**
 * A bucket-name prefix, in the intersection of what S3 and Cloud Storage allow.
 *
 * Stricter than either on purpose: this prefix is a security boundary — the
 * API's bucket-lifecycle grant is written against `<prefix>-*` — so a value
 * that could be read two ways by two clouds is not one to accept.
 */
const BUCKET_PREFIX = /^[a-z0-9][a-z0-9-]{1,40}$/

const DATABASE_SIZES: DatabaseSize[] = ['small', 'medium']
const CACHE_SIZES: CacheSize[] = ['small', 'medium']
const RUNNER_SIZES: RunnerSize[] = ['small', 'medium', 'large']
const CLICKHOUSE_MODES: ClickHouseMode[] = ['self-hosted', 'managed', 'disabled']

/** RDS keeps at most 35 days of automated backups; below one means none. */
const MAX_BACKUP_RETENTION_DAYS = 35

/** A root disk small enough to hold no box image is not worth deploying. */
const DISK_BOUNDS = { min: 20, max: 4_000 }

const ALARM_NAMES: (keyof AlarmRequest)[] = ['apiServerErrors', 'proxyUnhealthyTargets', 'runnersUnreachable']

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
    throw new DeployConfigError(`${where} must be an object`)
  }
  return value as Record<string, unknown>
}

/**
 * Every key is known, and — outside a stage override — every key is present.
 *
 * The second half is what makes the defaults complete. A stage says only what
 * differs, so a missing key there is the whole point; a missing key in the
 * defaults is a value nothing supplies, and the resource it belongs to would be
 * created from whatever the provider's own default happened to be.
 */
const assertKeys = (
  block: Record<string, unknown>,
  known: readonly string[],
  where: string,
  { partial }: { partial: boolean },
): void => {
  const unknown = Object.keys(block).filter((key) => !known.includes(key))
  if (unknown.length > 0) {
    throw new DeployConfigError(`${where} does not take ${unknown.join(', ')}. It takes ${known.join(', ')}`)
  }
  if (partial) return
  const missing = known.filter((key) => !(key in block))
  if (missing.length > 0) throw new DeployConfigError(`${where} must set ${missing.join(', ')}`)
}

const assertBoolean = (value: unknown, where: string): boolean => {
  if (typeof value !== 'boolean') throw new DeployConfigError(`${where} must be true or false`)
  return value
}

const assertWholeNumber = (value: unknown, where: string, { min, max }: { min: number; max: number }): number => {
  if (typeof value !== 'number' || !Number.isInteger(value) || value < min || value > max) {
    throw new DeployConfigError(`${where} must be a whole number from ${min} to ${max}`)
  }
  return value
}

const assertOneOf = <T extends string>(value: unknown, allowed: readonly T[], where: string): T => {
  if (typeof value !== 'string' || !allowed.includes(value as T)) {
    throw new DeployConfigError(`${where} must be one of ${allowed.join(', ')}`)
  }
  return value as T
}

const assertPattern = (value: unknown, pattern: RegExp, where: string): string => {
  if (typeof value !== 'string' || !pattern.test(value)) {
    throw new DeployConfigError(`${where} must match ${pattern.source}`)
  }
  return value
}

const parseDatabase = (raw: unknown, where: string, { partial }: { partial: boolean }): Partial<DatabaseRequest> => {
  const block = assertObject(raw, where)
  const known = ['name', 'size', 'highlyAvailable', 'backupRetentionDays', 'protected'] as const
  assertKeys(block, known, where, { partial })
  const parsed: Partial<DatabaseRequest> = {}
  if ('name' in block) parsed.name = assertPattern(block.name, UNQUOTED_NAME, `${where}.name`)
  if ('size' in block) parsed.size = assertOneOf(block.size, DATABASE_SIZES, `${where}.size`)
  if ('highlyAvailable' in block) {
    parsed.highlyAvailable = assertBoolean(block.highlyAvailable, `${where}.highlyAvailable`)
  }
  if ('backupRetentionDays' in block) {
    parsed.backupRetentionDays = assertWholeNumber(block.backupRetentionDays, `${where}.backupRetentionDays`, {
      min: 0,
      max: MAX_BACKUP_RETENTION_DAYS,
    })
  }
  if ('protected' in block) parsed.protected = assertBoolean(block.protected, `${where}.protected`)
  return parsed
}

const parseCache = (raw: unknown, where: string, { partial }: { partial: boolean }): Partial<CacheRequest> => {
  const block = assertObject(raw, where)
  assertKeys(block, ['size', 'clustered', 'encryptInTransit'] as const, where, { partial })
  const parsed: Partial<CacheRequest> = {}
  if ('size' in block) parsed.size = assertOneOf(block.size, CACHE_SIZES, `${where}.size`)
  if ('clustered' in block) parsed.clustered = assertBoolean(block.clustered, `${where}.clustered`)
  if ('encryptInTransit' in block) {
    const encrypted = assertBoolean(block.encryptInTransit, `${where}.encryptInTransit`)
    // Refused rather than accepted and ignored. The cache carries session state
    // and box credentials across a network shared with every other workload, so
    // "off" is not a trade-off this repository offers — and a setting that was
    // read, disallowed, and silently overridden would read as if it worked.
    if (!encrypted) throw new DeployConfigError(`${where}.encryptInTransit cannot be false; the cache holds sessions`)
    parsed.encryptInTransit = encrypted
  }
  return parsed
}

const parseStorage = (raw: unknown, where: string, { partial }: { partial: boolean }): Partial<StorageRequest> => {
  const block = assertObject(raw, where)
  assertKeys(block, ['volumePrefix', 'versioning'] as const, where, { partial })
  const parsed: Partial<StorageRequest> = {}
  if ('volumePrefix' in block) {
    parsed.volumePrefix = assertPattern(block.volumePrefix, BUCKET_PREFIX, `${where}.volumePrefix`)
  }
  if ('versioning' in block) parsed.versioning = assertBoolean(block.versioning, `${where}.versioning`)
  return parsed
}

const parseClickHouse = (
  raw: unknown,
  where: string,
  { partial }: { partial: boolean },
): Partial<ClickHouseRequest> => {
  const block = assertObject(raw, where)
  const known = ['mode', 'database', 'writerUsername', 'readerUsername', 'instanceSize', 'dataGb'] as const
  assertKeys(block, known, where, { partial })
  const parsed: Partial<ClickHouseRequest> = {}
  if ('mode' in block) parsed.mode = assertOneOf(block.mode, CLICKHOUSE_MODES, `${where}.mode`)
  for (const key of ['database', 'writerUsername', 'readerUsername'] as const) {
    if (key in block) parsed[key] = assertPattern(block[key], UNQUOTED_NAME, `${where}.${key}`)
  }
  if ('instanceSize' in block) {
    parsed.instanceSize = assertOneOf(block.instanceSize, ['small', 'medium'] as const, `${where}.instanceSize`)
  }
  if ('dataGb' in block) parsed.dataGb = assertWholeNumber(block.dataGb, `${where}.dataGb`, DISK_BOUNDS)
  if (parsed.writerUsername && parsed.writerUsername === parsed.readerUsername) {
    // One credential for both would mean a compromised read path could rewrite
    // the history it is reading, which is the whole reason there are two.
    throw new DeployConfigError(`${where}: writerUsername and readerUsername must differ`)
  }
  return parsed
}

const parseRunners = (raw: unknown, where: string, { partial }: { partial: boolean }): Partial<RunnerSettings> => {
  const block = assertObject(raw, where)
  assertKeys(block, ['size', 'rootDiskGb'] as const, where, { partial })
  const parsed: Partial<RunnerSettings> = {}
  if ('size' in block) parsed.size = assertOneOf(block.size, RUNNER_SIZES, `${where}.size`)
  if ('rootDiskGb' in block) parsed.rootDiskGb = assertWholeNumber(block.rootDiskGb, `${where}.rootDiskGb`, DISK_BOUNDS)
  return parsed
}

const parseAlarms = (raw: unknown, where: string, { partial }: { partial: boolean }): Partial<AlarmRequest> => {
  const block = assertObject(raw, where)
  assertKeys(block, ALARM_NAMES, where, { partial })
  const parsed: Partial<AlarmRequest> = {}
  for (const name of ALARM_NAMES) {
    if (!(name in block)) continue
    const alarm = assertObject(block[name], `${where}.${name}`)
    assertKeys(alarm, ['threshold', 'periods'] as const, `${where}.${name}`, { partial: false })
    parsed[name] = {
      threshold: assertWholeNumber(alarm.threshold, `${where}.${name}.threshold`, { min: 1, max: 1_000_000 }),
      periods: assertWholeNumber(alarm.periods, `${where}.${name}.periods`, { min: 1, max: 100 }),
    } satisfies AlarmThreshold
  }
  return parsed
}

const PARSERS = {
  database: parseDatabase,
  cache: parseCache,
  storage: parseStorage,
  clickhouse: parseClickHouse,
  runners: parseRunners,
  alarms: parseAlarms,
} as const

export const parseDeployConfig = (path: string, contents: string): DeployConfig => {
  let raw: unknown
  try {
    raw = JSON.parse(contents)
  } catch (error) {
    throw new DeployConfigError(`${path} is not valid JSON: ${(error as Error).message}`)
  }
  const root = assertObject(raw, path)
  assertKeys(root, [...SECTIONS, 'stages'], path, { partial: true })
  const defaults = Object.fromEntries(
    SECTIONS.map((section) => [section, PARSERS[section](root[section], `${path}: "${section}"`, { partial: false })]),
  ) as Pick<DeployConfig, (typeof SECTIONS)[number]>

  const stages: DeployConfig['stages'] = {}
  if ('stages' in root) {
    const declared = assertObject(root.stages, `${path}: "stages"`)
    for (const [name, value] of Object.entries(declared)) {
      if (!STAGE_NAME.test(name)) {
        throw new DeployConfigError(`${path}: stage "${name}" may only contain letters, digits and "-"`)
      }
      const where = `${path}: "stages.${name}`
      const block = assertObject(value, `${where}"`)
      assertKeys(block, SECTIONS, `${where}"`, { partial: true })
      stages[name] = Object.fromEntries(
        SECTIONS.map((section) => [
          section,
          PARSERS[section](block[section] ?? {}, `${where}.${section}"`, { partial: true }),
        ]),
      ) as StageOverride
    }
  }
  return { path, root: dirname(path), ...defaults, stages }
}

/**
 * What one stage deploys into: the defaults with that stage's overrides on top.
 * A stage this file never mentions is not an error — it simply changes nothing,
 * which is what "the same as everywhere else" should look like.
 */
export const databaseFor = (config: DeployConfig, stage: string): DatabaseRequest => ({
  ...config.database,
  ...config.stages[stage]?.database,
})

export const cacheFor = (config: DeployConfig, stage: string): CacheRequest => ({
  ...config.cache,
  ...config.stages[stage]?.cache,
})

export const storageFor = (config: DeployConfig, stage: string): StorageRequest => ({
  ...config.storage,
  ...config.stages[stage]?.storage,
})

export const clickHouseFor = (config: DeployConfig, stage: string): ClickHouseRequest => ({
  ...config.clickhouse,
  ...config.stages[stage]?.clickhouse,
})

export const runnersFor = (config: DeployConfig, stage: string): RunnerSettings => ({
  ...config.runners,
  ...config.stages[stage]?.runners,
})

/** Alarms merge alarm by alarm; a stage that retunes one keeps the others. */
export const alarmsFor = (config: DeployConfig, stage: string): AlarmRequest => ({
  ...config.alarms,
  ...config.stages[stage]?.alarms,
})

export const loadDeployConfig = ({
  cwd = process.cwd(),
  environment = process.env,
}: { cwd?: string; environment?: NodeJS.ProcessEnv } = {}): DeployConfig => {
  const override = environment.MDEPLOY_CONFIG
  const path = override ? (isAbsolute(override) ? override : resolve(cwd, override)) : findUp(cwd, CONFIG_FILENAME)
  if (!path) throw new DeployConfigError(`Could not find ${CONFIG_FILENAME} in ${cwd} or any parent directory`)
  let contents: string
  try {
    contents = readFileSync(path, 'utf8')
  } catch (error) {
    throw new DeployConfigError(`Could not read ${path}: ${(error as Error).message}`)
  }
  return parseDeployConfig(path, contents)
}
