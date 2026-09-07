/**
 * What may leave a stage's store, and the one way to ask for it.
 *
 * `mstage.config.json` declares named groups under `env.selectGroup`; a consumer names a
 * group and gets exactly what that group holds. It never carries a list of its
 * own: two declarations of one set drift silently — a key added to the group
 * would never be read, and one added to the copy would never be exported — and
 * the drift is only ever found by a test written to look for it.
 *
 * That makes adding a key to an export a reviewable edit to one file, which is
 * the only reason exporting is safe at all.
 */

import { loadConfig, type MstageConfig } from '../config/load.ts'
import { ambientClients, readEnvironment, type Clients, type StoreBackend } from './store.ts'

export class ExportError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ExportError'
  }
}

/** The keys one declared group names, or an error naming the groups that exist. */
export const groupKeys = ({
  group,
  groups,
  where,
}: {
  group: string
  groups: Record<string, string[]>
  where: string
}): string[] => {
  const keys = groups[group]
  if (keys) return keys
  const known = Object.keys(groups)
  throw new ExportError(
    known.length > 0
      ? `${where} declares no "${group}" under env.selectGroup. Declared: ${known.join(', ')}`
      : `${where} declares no env.selectGroup at all`,
  )
}

/**
 * Narrows a store to one declared group.
 *
 * A key the group names but the store does not hold is an error rather than an
 * omission: a process handed a silently short environment fails later, somewhere
 * that does not mention the missing key.
 *
 * `optional` names members that need not be there yet, for a caller that is
 * about to produce one. Exporting never has that case; deriving the group's own
 * digest does, because the digest is a member of the group it describes.
 */
export const valuesOfGroup = ({
  group,
  groups,
  values,
  where,
  optional = [],
}: {
  group: string
  groups: Record<string, string[]>
  values: Record<string, string>
  where: string
  optional?: string[]
}): Record<string, string> => {
  const keys = groupKeys({ group, groups, where })
  const missing = keys.filter((key) => !(key in values) && !optional.includes(key))
  if (missing.length > 0) {
    throw new ExportError(`the store is missing ${missing.join(', ')}, which env.selectGroup.${group} names`)
  }
  return Object.fromEntries(keys.filter((key) => key in values).map((key) => [key, values[key] as string]))
}

/**
 * One group's keys and values, read from a stage's store.
 *
 * This is what a server or a deploy calls. It hands back the values and does
 * nothing with them: whether they belong in `process.env`, in a child process,
 * or in a file is the caller's decision, and mstage has no business making it
 * from inside a library.
 *
 * `versionId` reads the object as some earlier moment saw it. A deploy records
 * the version it shipped and passes it here, so a task that starts again hours
 * later reads the configuration the deploy was built against rather than
 * whatever the store holds by then.
 */
export const selectGroup = async ({
  group,
  stage,
  region,
  app,
  versionId,
  clients,
  config = loadConfig(),
}: {
  group: string
  stage: string
  region?: string
  app?: string
  versionId?: string
  /**
   * A caller that already built AWS clients passes those; one on another cloud
   * passes the backend it built. `readEnvironment` has taken either since the
   * store stopped knowing which cloud it was reading, and narrowing it here to
   * only the AWS half is what kept a GCP stage from reaching this at all.
   */
  clients?: Clients | StoreBackend
  config?: MstageConfig
}): Promise<Record<string, string>> => {
  if (!stage) throw new ExportError('selectGroup needs the stage whose store to read')
  if (!clients && !region) throw new ExportError('selectGroup needs a region, or clients already built for one')

  const values = await readEnvironment({
    clients: clients ?? ambientClients(region as string),
    app: app ?? config.app,
    stage,
    ...(versionId ? { versionId } : {}),
  })
  return valuesOfGroup({
    group,
    groups: config.envSelectGroup,
    values,
    where: config.path,
    // What the declaration said may be absent. Read from the config rather than
    // taken as an argument: a caller that supplied its own list would be a
    // second answer to a question this file already asked.
    optional: config.envOptional[group] ?? [],
  })
}
