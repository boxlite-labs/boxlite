/*
 * The store on GCP: one bucket, one secret per stage — and, in the same bucket,
 * the state the deploy engine keeps.
 *
 * The store's own layout is mstage's, chosen to be boring, because nothing else
 * reads it:
 *
 *   Secret Manager  mstage-bootstrap                   which bucket holds the store
 *   GCS             secret/<app>/<stage>.json          the sealed map
 *   Secret Manager  mstage-passphrase-<app>-<stage>    the key
 *
 * The deployment objects below `state` are not mstage's to choose. Pulumi
 * deploys a GCP stage (`mdeploy/src/pulumi.ts`) and keeps its checkpoint and
 * locks in this bucket, so those keys are its layout and are read exactly as it
 * writes them — the same relationship the AWS backend has with SST, against a
 * different engine.
 *
 * The bucket is discovered rather than passed, the same way AWS reads
 * `/sst/bootstrap`. Not for obscurity — IAM is the boundary either way — but so
 * that moving the store to another bucket is one edit to one record instead of
 * an edit everywhere a caller constructs a backend. The record has the same
 * shape on both clouds, `{"state": "<bucket>"}`, so the two lookups read alike.
 *
 * Object versions are generation numbers, which GCS returns as integers. They
 * are carried as strings so a pinned version means the same thing to a caller on
 * either cloud.
 */

import { EnvError, objectKey, type StoreBackend, type StoredVersion } from './backend.ts'

/**
 * The two Google clients this needs, in the shape their SDKs already have.
 * Structural rather than imported so nothing here depends on the packages
 * being installed until a GCP stage actually exists.
 */
export type GcpClients = {
  storage: {
    bucket: (name: string) => {
      file: (
        path: string,
        options?: { generation?: number },
      ) => {
        download: () => Promise<[Buffer]>
        save: (data: Buffer, options?: { contentType?: string }) => Promise<void>
        getMetadata: () => Promise<[{ generation?: string | number; updated?: string; size?: string | number }]>
        /** Only the lock files are ever deleted; the store keeps every version. */
        delete: () => Promise<unknown>
      }
      getFiles: (options: {
        prefix: string
        /** Omitted when listing distinct keys rather than one object's history. */
        versions?: boolean
      }) => Promise<[{ name: string; metadata: Record<string, unknown> }[]]>
    }
  }
  secrets: {
    accessSecretVersion: (request: { name: string }) => Promise<[{ payload?: { data?: Uint8Array | string } }]>
  }
}

const isNotFound = (error: unknown): boolean => {
  const code = (error as { code?: number | string })?.code
  // 404 from Storage, 5 (NOT_FOUND) from the Secret Manager gRPC client.
  return code === 404 || code === 5 || code === 'ENOENT'
}

const BOOTSTRAP_SECRET = 'mstage-bootstrap'

/*
 * Where the engine keeps this stage's deployment state, which is not where SST
 * keeps it.
 *
 * On AWS the engine is SST and writes `app/<app>/<stage>.json` with a single
 * `lock/<app>/<stage>.json` beside it. On GCP the engine is Pulumi itself
 * (`mdeploy/src/pulumi.ts`), and its own backend keeps everything under
 * `.pulumi/` — so the same bucket holds the store and the checkpoint, which is
 * the whole reason a GCP stage needs no second cloud to deploy.
 *
 * Two differences are worth naming rather than smoothing over. The stack path
 * is scoped by project, which is what Pulumi writes into a new or empty backend
 * from 3.61.0 on and therefore what the bucket `iam/src/gcp.ts` creates gets; a
 * backend upgraded from the older flat layout is not read here, because nothing
 * in this repository makes one. And a lock is a *directory* — Pulumi writes one
 * file per operation holding the stage, named by a unique id — where SST has a
 * single object.
 *
 * `app` is the Pulumi project: mdeploy passes the app name as `projectName` and
 * the stage as `stackName`, so the two halves line up with SST's keys.
 */
const PULUMI = '.pulumi'

/**
 * `.pulumi/stacks/<project>/<stack>.json` — `projectReferenceStore.StackBasePath`
 * joins the stacks directory, the project and the stack name, and nothing else
 * (`pkg/backend/diy/store.go`).
 */
const checkpointKey = (app: string, stage: string): string => `${PULUMI}/stacks/${app}/${stage}.json`

/**
 * `.pulumi/locks/organization/<project>/<stack>/` — and the extra segment is not
 * a typo.
 *
 * Locks are keyed by `FullyQualifiedName()`, which renders a project-scoped
 * reference as `organization/<project>/<stack>` (`pkg/backend/diy/backend.go`),
 * and `lockPath` joins that under the locks directory
 * (`pkg/backend/diy/lock.go`). Stacks are keyed by the store instead, which
 * omits it. The two paths really are asymmetric; Pulumi's own DIY-backend
 * documentation describes the lock path without the segment, so reading the
 * docs rather than the source is how this gets written wrong — and wrong here
 * means listing an empty prefix and reporting a locked stage as free.
 */
const lockPrefix = (app: string, stage: string): string => `${PULUMI}/locks/organization/${app}/${stage}/`

/**
 * The lock files this stage currently has, by key.
 *
 * Listed rather than addressed: the name of each is a unique id the engine
 * chose, so there is nothing to construct. Distinct keys, not one object's
 * history, which is why `versions` is left off.
 */
const lockFiles = async (clients: GcpClients, project: string, app: string, stage: string): Promise<string[]> => {
  const bucket = await readStateBucket(clients, project)
  const [files] = await clients.storage.bucket(bucket).getFiles({ prefix: lockPrefix(app, stage) })
  return files.map((file) => file.name).sort()
}

/** Reads one Secret Manager version, or reports what is missing. */
const secretValue = async (clients: GcpClients, name: string): Promise<string> => {
  let answer
  try {
    ;[answer] = await clients.secrets.accessSecretVersion({ name })
  } catch (error) {
    if (isNotFound(error)) throw new EnvError(`${name} does not exist`)
    throw error
  }
  const data = answer.payload?.data
  if (data === undefined) throw new EnvError(`${name} holds no value`)
  return typeof data === 'string' ? data : Buffer.from(data).toString('utf8')
}

/**
 * Which bucket holds the store, for this project.
 *
 * The GCP counterpart of `/sst/bootstrap`. One record, read once per call, so
 * pointing the store at a different bucket is an edit to that record and
 * nothing else.
 */
export const readStateBucket = async (clients: GcpClients, project: string): Promise<string> => {
  const name = `projects/${project}/secrets/${BOOTSTRAP_SECRET}/versions/latest`
  const raw = await secretValue(clients, name)
  let bootstrap: { state?: string }
  try {
    bootstrap = JSON.parse(raw)
  } catch {
    throw new EnvError(`${name} is not valid JSON`)
  }
  if (!bootstrap.state) throw new EnvError(`${name} names no state bucket`)
  return bootstrap.state
}

export const gcpBackend = ({ clients, project }: { clients: GcpClients; project: string }): StoreBackend => ({
  home: 'gcp',

  async read({ app, stage, versionId }) {
    const key = objectKey(app, stage)
    const generation = versionId === undefined ? undefined : Number(versionId)
    if (generation !== undefined && !Number.isInteger(generation)) {
      throw new EnvError(`"${versionId}" is not a GCS generation; versions on this backend are integers`)
    }
    try {
      const [payload] = await clients.storage
        .bucket(await readStateBucket(clients, project))
        .file(key, generation === undefined ? undefined : { generation })
        .download()
      return payload.length === 0 ? null : payload
    } catch (error) {
      if (!isNotFound(error)) throw error
      // Same rule as the other backend: an unwritten stage is empty, a pinned
      // version that has gone is a failure.
      if (versionId) throw new EnvError(`${key} has no version ${versionId}; it was deleted or expired`)
      return null
    }
  },

  async write({ app, stage, sealed }) {
    await clients.storage
      .bucket(await readStateBucket(clients, project))
      .file(objectKey(app, stage))
      .save(sealed, { contentType: 'application/json' })
  },

  async currentVersion({ app, stage }) {
    try {
      const bucket = await readStateBucket(clients, project)
      const [metadata] = await clients.storage.bucket(bucket).file(objectKey(app, stage)).getMetadata()
      return metadata.generation === undefined ? null : String(metadata.generation)
    } catch (error) {
      if (isNotFound(error)) return null
      throw error
    }
  },

  async versions({ app, stage }) {
    const key = objectKey(app, stage)
    const bucket = await readStateBucket(clients, project)
    const [files] = await clients.storage.bucket(bucket).getFiles({ prefix: key, versions: true })
    const found: StoredVersion[] = []
    for (const file of files) {
      // A prefix listing, so anything sharing the leading path comes back too.
      if (file.name !== key) continue
      const metadata = file.metadata
      const deleted = metadata.timeDeleted !== undefined
      found.push({
        versionId: String(metadata.generation ?? ''),
        // GCS keeps a noncurrent version rather than writing a tombstone, so a
        // deleted generation is the closest thing to a delete marker.
        type: deleted ? 'delete marker' : 'version',
        lastModified: metadata.updated ? new Date(String(metadata.updated)) : null,
        size: deleted ? null : Number(metadata.size ?? 0),
        storageClass: deleted ? null : ((metadata.storageClass as string | undefined) ?? null),
      })
    }
    return found.sort((left, right) => (right.lastModified?.getTime() ?? 0) - (left.lastModified?.getTime() ?? 0))
  },

  async passphrase({ app, stage }) {
    const name = `projects/${project}/secrets/mstage-passphrase-${app}-${stage}/versions/latest`
    // The value is the same base64 the other backend stores, so either opens an
    // object sealed by the other.
    try {
      return Buffer.from(await secretValue(clients, name), 'base64')
    } catch (error) {
      if (error instanceof EnvError && error.message.endsWith('does not exist')) {
        throw new EnvError(`${name} does not exist, so this store cannot be decrypted`)
      }
      throw error
    }
  },

  state: {
    async readCheckpoint({ app, stage }) {
      const bucket = await readStateBucket(clients, project)
      try {
        const [payload] = await clients.storage.bucket(bucket).file(checkpointKey(app, stage)).download()
        return payload.length === 0 ? null : payload
      } catch (error) {
        if (isNotFound(error)) return null
        throw error
      }
    },

    async writeCheckpoint({ app, stage, checkpoint }) {
      const bucket = await readStateBucket(clients, project)
      await clients.storage
        .bucket(bucket)
        .file(checkpointKey(app, stage))
        .save(checkpoint, { contentType: 'application/json' })
    },

    async readLock({ app, stage }) {
      const held = await lockFiles(clients, project, app, stage)
      if (held.length === 0) return null
      /*
       * Refused rather than half-answered. Each file is one operation holding
       * the stage, so two files are two holders, and reporting one of them
       * would name a holder nobody asked about — and would quietly break the
       * check in `state/store.ts` that the lock being dropped is the lock that
       * was named. The names are not printed: the caller is told how many, and
       * `unlock` removes them all once it knows.
       */
      if (held.length > 1) {
        throw new EnvError(
          `${app}/${stage} has ${held.length} locks, so no single one holds it. ` +
            'Read them in the bucket before dropping any',
        )
      }
      const bucket = await readStateBucket(clients, project)
      try {
        const [payload] = await clients.storage.bucket(bucket).file(held[0]!).download()
        return payload.length === 0 ? null : payload
      } catch (error) {
        // Released between the listing and this read: no lock, which is the
        // answer the caller wanted. Guarded like the two reads either side of
        // it — an unguarded 404 leaves here as a GCS message naming the bucket,
        // and `bin/mstage.ts` prints what reaches it.
        if (isNotFound(error)) return null
        throw error
      }
    },

    async removeLock({ app, stage }) {
      const bucket = await readStateBucket(clients, project)
      // Every one of them: a stage with any lock file left is a stage the next
      // deploy still refuses, so removing one of two would report success and
      // change nothing a caller can see.
      for (const key of await lockFiles(clients, project, app, stage)) {
        try {
          await clients.storage.bucket(bucket).file(key).delete()
        } catch (error) {
          // A lock released between the listing and here is the outcome asked
          // for, not a failure.
          if (!isNotFound(error)) throw error
        }
      }
    },
  },
})
