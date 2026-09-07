/*
 * A stage's configuration, whichever cloud keeps it.
 *
 * Everything below this comment is the same on every cloud: how a value is
 * sealed, what a name may be, that a write is one read-modify-write, that a
 * missing key is reported rather than raised. Where the bytes actually live is
 * a `StoreBackend`, and this module never learns which one it was handed.
 *
 * The split is not speculative. mstage is meant to serve three repositories,
 * and the platform intends to be able to leave a cloud without rewriting how
 * configuration works — so the part that would have to be rewritten is the part
 * that is behind an interface. Before it, `sst secret`'s S3 and SSM layout was
 * the only thing this module knew how to talk to.
 */

import { EnvError, objectKey, open, seal, type StoreBackend, type StoredVersion } from './backend.ts'
import { awsBackend, ambientClients as awsAmbientClients, clientsFor as awsClientsFor } from './aws-backend.ts'
import type { AwsIdentity } from '../aws/identity.ts'

export { EnvError, type StoreBackend, type StoredVersion }
export { readStateBucket } from './aws-backend.ts'

/**
 * The AWS clients, kept exported under their old names because every caller in
 * this repository builds them. A caller that wants another cloud builds that
 * cloud's backend instead and passes it directly.
 */
export type Clients = ReturnType<typeof awsClientsFor>
export const clientsFor = (identity: Pick<AwsIdentity, 'credentials' | 'region'>): Clients => awsClientsFor(identity)
export const ambientClients = (region: string): Clients => awsAmbientClients(region)

/**
 * Callers still hand over AWS clients, so this is where they become a backend.
 * The overload keeps every existing call site unchanged while letting a GCP
 * caller pass a backend it built itself.
 */
const backendFrom = (source: Clients | StoreBackend): StoreBackend =>
  'home' in source ? source : awsBackend(source)

/** One stage's map. A stage that was never written is empty rather than an error. */
export const readEnvironment = async ({
  clients,
  app,
  stage,
  versionId,
}: {
  clients: Clients | StoreBackend
  app: string
  stage: string
  /** Read the object as it was, not as it is. See `currentVersion`. */
  versionId?: string
}): Promise<Record<string, string>> => {
  const backend = backendFrom(clients)
  const sealed = await backend.read({ app, stage, ...(versionId ? { versionId } : {}) })
  if (!sealed) return {}
  const key = objectKey(app, stage)
  const parsed: unknown = JSON.parse(open(sealed, await backend.passphrase({ app, stage }), key))
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new EnvError(`${key} did not decrypt to an object`)
  }
  return parsed as Record<string, string>
}

/**
 * Which version of a stage's object is current.
 *
 * A deploy records this and hands it to what it deploys, so a task that starts
 * again hours later reads the configuration the deploy was built against.
 * Without it a restart is a silent second deploy of someone else's edit.
 */
export const currentVersion = async ({
  clients,
  app,
  stage,
}: {
  clients: Clients | StoreBackend
  app: string
  stage: string
}): Promise<string | null> => backendFrom(clients).currentVersion({ app, stage })

/**
 * Every version of a stage's object, newest first.
 *
 * Delete markers are listed with the versions rather than filtered out: a stage
 * that reads as empty is usually explained by one, and hiding it would leave
 * that unexplained.
 */
export const listVersions = async ({
  clients,
  app,
  stage,
}: {
  clients: Clients | StoreBackend
  app: string
  stage: string
}): Promise<StoredVersion[]> => backendFrom(clients).versions({ app, stage })

/** Replaces one stage's map, sealed the way every backend reads it. */
export const writeStage = async ({
  clients,
  app,
  stage,
  values,
}: {
  clients: Clients | StoreBackend
  app: string
  stage: string
  values: Record<string, string>
}): Promise<void> => {
  const backend = backendFrom(clients)
  const key = objectKey(app, stage)
  const sealed = seal(JSON.stringify(values), await backend.passphrase({ app, stage }), key)
  await backend.write({ app, stage, sealed })
}

/** SST's constraint on a name set through `secret set` (cmd/sst/secret.go:363). */
export const SECRET_NAME = /^[A-Z][a-zA-Z0-9_]*$/

export type WriteOutcome = { name: string; existed: boolean; unchanged: boolean }

/**
 * Sets one or more keys in a single read-modify-write.
 *
 * One write rather than one per key: the store is a single object, so writing
 * per key would cost a round trip each and widen the window in which a
 * concurrent writer loses somebody's change.
 *
 * `derive` runs after the assignments and before the write, for a value that
 * depends on the others — a digest over the result cannot be computed until the
 * result exists, and must not need a second write to land.
 */
export const setValues = async ({
  clients,
  app,
  stage,
  entries,
  derive,
}: {
  clients: Clients | StoreBackend
  app: string
  stage: string
  entries: [string, string][]
  derive?: (values: Record<string, string>) => [string, string][]
}): Promise<{ outcomes: WriteOutcome[] }> => {
  for (const [name] of entries) {
    if (!SECRET_NAME.test(name)) {
      throw new EnvError(`"${name}" is not a usable name; SST requires ${SECRET_NAME.source}`)
    }
  }
  const backend = backendFrom(clients)
  const current = await readEnvironment({ clients: backend, app, stage })

  const next = { ...current }
  const outcomes: WriteOutcome[] = []
  const record = (name: string, value: string) => {
    outcomes.push({ name, existed: Object.hasOwn(current, name), unchanged: current[name] === value })
    next[name] = value
  }
  for (const [name, value] of entries) record(name, value)
  for (const [name, value] of derive?.(next) ?? []) record(name, value)

  if (outcomes.some((outcome) => !outcome.unchanged)) {
    await writeStage({ clients: backend, app, stage, values: next })
  }
  return { outcomes }
}

export type DeleteOutcome = { name: string; existed: boolean }

/**
 * Removes keys, however many, in a single read-modify-write.
 *
 * One write rather than one per key, for the same reason `setValues` makes one:
 * the store is a single object, so removing them one at a time would cost a
 * round trip each and widen the window in which a concurrent writer loses
 * somebody's change.
 *
 * A key that was not there is reported rather than treated as a failure: the
 * caller asked for it to be gone, and it is. When none of the names was there
 * the object is not resealed at all.
 *
 * Nothing is derived here, unlike `setValues`. A digest describes the keys of
 * one group, and a removal either leaves that group alone — in which case the
 * stored fingerprint still describes it — or takes a member out of it, which no
 * recomputation can make true again while the group still names that member.
 */
export const deleteValues = async ({
  clients,
  app,
  stage,
  names,
}: {
  clients: Clients | StoreBackend
  app: string
  stage: string
  names: readonly string[]
}): Promise<{ outcomes: DeleteOutcome[] }> => {
  const backend = backendFrom(clients)
  const current = await readEnvironment({ clients: backend, app, stage })
  // `hasOwn`, not `in`, for the reason `setValues` uses it: the map came out of
  // `JSON.parse`, so it inherits `toString` and the rest of Object.prototype.
  const outcomes = names.map((name) => ({ name, existed: Object.hasOwn(current, name) }))
  if (outcomes.some((outcome) => outcome.existed)) {
    const rest = Object.fromEntries(Object.entries(current).filter(([key]) => !names.includes(key)))
    await writeStage({ clients: backend, app, stage, values: rest })
  }
  return { outcomes }
}
