/*
 * The store on AWS, in SST v3.19.3's own layout.
 *
 * This backend does not get to choose how things are named. `sst deploy` reads
 * and writes the same objects, so every path here matches what SST does —
 * `pkg/project/provider/aws.go:541` and `:545` — and a change to any of them
 * would give this platform two stores that disagree.
 *
 *   SSM  /sst/bootstrap                    → the state bucket's name
 *   S3   secret/<app>/<stage>.json         → the sealed map
 *   SSM  /sst/passphrase/<app>/<stage>     → the key
 *   S3   app/<app>/<stage>.json            → the deployment checkpoint
 *   S3   lock/<app>/<stage>.json           → the lock a deploy holds on it
 *
 * The last two are what a stopped deploy leaves behind. SST names every one of
 * these objects `<kind>/<app>/<stage>.json` through the same `pathForData`, and
 * picks the kind at the call site — `app` in `PullState` and `PushPartialState`,
 * `lock` in `Lock` and `Unlock` (`pkg/project/provider/provider.go`).
 *
 * The bucket's name ends in twelve characters chosen at bootstrap, so nothing
 * but that first parameter knows it. It is never logged, on success or in an
 * error: a log is read by more people than an account is, and one line of
 * scrollback would hand over the name that guesswork cannot reach. Objects are
 * named by their key instead, which the app and stage already gave.
 */

import {
  DeleteObjectCommand,
  GetObjectCommand,
  HeadObjectCommand,
  ListObjectVersionsCommand,
  PutObjectCommand,
  S3Client,
} from '@aws-sdk/client-s3'
import { GetParameterCommand, SSMClient } from '@aws-sdk/client-ssm'
import { EnvError, objectKey, type StoreBackend, type StoredVersion } from './backend.ts'
import type { AwsIdentity } from '../aws/identity.ts'

const BOOTSTRAP_PARAMETER = '/sst/bootstrap'

export type AwsClients = {
  s3: { send: (command: any) => Promise<any> }
  ssm: { send: (command: any) => Promise<any> }
}

export const clientsFor = (identity: Pick<AwsIdentity, 'credentials' | 'region'>): AwsClients => {
  const config = { region: identity.region, credentials: identity.credentials }
  return { s3: new S3Client(config), ssm: new SSMClient(config) }
}

/**
 * Clients for a caller that has no mstage identity to hand — a container
 * holding a task role, where the SDK's own chain is already the right answer.
 */
export const ambientClients = (region: string): AwsClients => ({
  s3: new S3Client({ region }),
  ssm: new SSMClient({ region }),
})

const isNotFound = (error: unknown): boolean => {
  const name = (error as { name?: string })?.name
  return name === 'NoSuchKey' || name === 'NotFound' || name === 'ParameterNotFound'
}

/** The state bucket for this region. Nothing else records its random suffix. */
export const readStateBucket = async (clients: AwsClients): Promise<string> => {
  let answer
  try {
    answer = await clients.ssm.send(new GetParameterCommand({ Name: BOOTSTRAP_PARAMETER, WithDecryption: false }))
  } catch (error) {
    if (isNotFound(error)) {
      throw new EnvError(`${BOOTSTRAP_PARAMETER} does not exist in this region; the account was never bootstrapped`)
    }
    throw error
  }
  let bootstrap: { state?: string }
  try {
    bootstrap = JSON.parse(answer.Parameter?.Value ?? '')
  } catch {
    throw new EnvError(`${BOOTSTRAP_PARAMETER} is not valid JSON`)
  }
  if (!bootstrap.state) throw new EnvError(`${BOOTSTRAP_PARAMETER} names no state bucket`)
  return bootstrap.state
}

/** The other two objects SST keeps per stage, in the same `<kind>/<app>/<stage>.json` shape. */
const checkpointKey = (app: string, stage: string): string => `app/${app}/${stage}.json`
const lockKey = (app: string, stage: string): string => `lock/${app}/${stage}.json`

/** One object as stored, or null when it is not there. Neither of these is sealed. */
const readObject = async (clients: AwsClients, key: string): Promise<Buffer | null> => {
  const bucket = await readStateBucket(clients)
  let answer
  try {
    answer = await clients.s3.send(new GetObjectCommand({ Bucket: bucket, Key: key }))
  } catch (error) {
    if (isNotFound(error)) return null
    throw error
  }
  const payload = Buffer.from(await answer.Body.transformToByteArray())
  return payload.length === 0 ? null : payload
}

export const awsBackend = (clients: AwsClients): StoreBackend => ({
  home: 'aws',

  async read({ app, stage, versionId }) {
    const bucket = await readStateBucket(clients)
    const key = objectKey(app, stage)
    let answer
    try {
      answer = await clients.s3.send(new GetObjectCommand({ Bucket: bucket, Key: key, VersionId: versionId }))
    } catch (error) {
      if (!isNotFound(error)) throw error
      // A stage nobody has written yet is empty rather than an error, which is
      // what SST reports too. A *pinned* version that has gone is not the same
      // thing: answering empty there would start a caller with no configuration
      // at exactly the moment it asked for a specific one.
      if (versionId) throw new EnvError(`${key} has no version ${versionId}; it was deleted or expired`)
      return null
    }
    const payload = Buffer.from(await answer.Body.transformToByteArray())
    return payload.length === 0 ? null : payload
  },

  async write({ app, stage, sealed }) {
    const bucket = await readStateBucket(clients)
    await clients.s3.send(
      new PutObjectCommand({
        Bucket: bucket,
        Key: objectKey(app, stage),
        Body: sealed,
        ContentType: 'application/json',
      }),
    )
  },

  async currentVersion({ app, stage }) {
    const bucket = await readStateBucket(clients)
    let answer
    try {
      answer = await clients.s3.send(new HeadObjectCommand({ Bucket: bucket, Key: objectKey(app, stage) }))
    } catch (error) {
      if (isNotFound(error)) return null
      throw error
    }
    const version = answer.VersionId
    // S3 reports the literal "null" for an object in an unversioned bucket.
    return typeof version === 'string' && version !== 'null' ? version : null
  },

  async versions({ app, stage }) {
    const bucket = await readStateBucket(clients)
    const key = objectKey(app, stage)
    const found: StoredVersion[] = []
    let keyMarker: string | undefined
    let versionIdMarker: string | undefined

    do {
      const answer = await clients.s3.send(
        new ListObjectVersionsCommand({
          Bucket: bucket,
          // A prefix, so anything sharing this leading path comes back too and
          // is dropped below. Only the exact object is this stage's.
          Prefix: key,
          KeyMarker: keyMarker,
          VersionIdMarker: versionIdMarker,
        }),
      )
      const collect = (entries: any[] | undefined, type: StoredVersion['type']) => {
        for (const entry of entries ?? []) {
          if (entry.Key !== key || typeof entry.VersionId !== 'string') continue
          found.push({
            versionId: entry.VersionId,
            type,
            lastModified: entry.LastModified ?? null,
            size: type === 'version' ? (entry.Size ?? null) : null,
            storageClass: type === 'version' ? (entry.StorageClass ?? null) : null,
          })
        }
      }
      collect(answer.Versions, 'version')
      collect(answer.DeleteMarkers, 'delete marker')
      keyMarker = answer.IsTruncated ? answer.NextKeyMarker : undefined
      versionIdMarker = answer.IsTruncated ? answer.NextVersionIdMarker : undefined
    } while (keyMarker || versionIdMarker)

    return found.sort((left, right) => (right.lastModified?.getTime() ?? 0) - (left.lastModified?.getTime() ?? 0))
  },

  async passphrase({ app, stage }) {
    const name = `/sst/passphrase/${app}/${stage}`
    let answer
    try {
      answer = await clients.ssm.send(new GetParameterCommand({ Name: name, WithDecryption: true }))
    } catch (error) {
      if (isNotFound(error)) throw new EnvError(`${name} does not exist, so this store cannot be decrypted`)
      throw error
    }
    return Buffer.from(answer.Parameter?.Value ?? '', 'base64')
  },

  state: {
    readCheckpoint: ({ app, stage }) => readObject(clients, checkpointKey(app, stage)),

    async writeCheckpoint({ app, stage, checkpoint }) {
      const bucket = await readStateBucket(clients)
      await clients.s3.send(
        new PutObjectCommand({
          Bucket: bucket,
          Key: checkpointKey(app, stage),
          Body: checkpoint,
          ContentType: 'application/json',
        }),
      )
    },

    readLock: ({ app, stage }) => readObject(clients, lockKey(app, stage)),

    async removeLock({ app, stage }) {
      const bucket = await readStateBucket(clients)
      // S3 deletes a key that was never there without complaint, so whether a
      // lock existed is the caller's question and is answered by reading first.
      await clients.s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: lockKey(app, stage) }))
    },
  },
})
