/*
 * What a cloud has to answer for mstage to keep a stage's configuration in it.
 *
 * Five questions, and none of them mentions a bucket, a parameter or a secret
 * version. That is the whole point: the layout is the backend's business, and
 * the two backends do not share one. On AWS the layout is SST's, because the
 * same objects are read and written by `sst deploy` and diverging from it would
 * mean two stores; anywhere else mstage is free to choose, and does.
 *
 * A backend answers for two more objects, `StateObjects` below, in whatever
 * layout the engine that deploys into its home uses. Grouped rather than asked
 * as two more questions, because they belong to that engine and the five above
 * belong to mstage.
 *
 * The encryption is not a backend's business either. Values are sealed before
 * they reach one and opened after they leave it, so a backend never holds a
 * readable secret and a second backend cannot get the format subtly wrong. What
 * a backend stores is opaque bytes it must return unchanged.
 */

import { createCipheriv, createDecipheriv, randomBytes } from 'node:crypto'

const GCM_NONCE_BYTES = 12
const GCM_TAG_BYTES = 16

export class EnvError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'EnvError'
  }
}

/** One stored revision of a stage's object. */
export type StoredVersion = {
  versionId: string
  /** What a store distinguishes: a stored object, or the tombstone hiding it. */
  type: 'version' | 'delete marker'
  lastModified: Date | null
  /** Absent for a delete marker, which holds nothing. */
  size: number | null
  storageClass: string | null
}

/**
 * The two things an engine keeps for a stage beside the store: a checkpoint,
 * and whatever it holds while rewriting one.
 *
 * Both exist only because something deploys. mstage does not, so it writes no
 * checkpoint of its own and takes no lock — it offers the two repairs a deploy
 * cannot make for itself, because both are needed exactly when a deploy stopped
 * halfway: dropping a lock the process did not live to release, and editing a
 * checkpoint whose pending operations refuse the next deploy.
 *
 * Deliberately says nothing about where either lives. The two engines disagree:
 * SST keeps `app/<app>/<stage>.json` with one `lock/…` object beside it, and
 * Pulumi keeps everything under `.pulumi/` with a *directory* of lock files.
 * Each backend answers for its own layout, and a caller works in checkpoints and
 * locks rather than keys.
 *
 * Neither object is sealed. Whatever is secret inside the checkpoint was
 * encrypted by Pulumi before it was stored, so these are bytes to carry
 * unchanged rather than something to open.
 */
export type StateObjects = {
  readCheckpoint: (input: { app: string; stage: string }) => Promise<Buffer | null>
  writeCheckpoint: (input: { app: string; stage: string; checkpoint: Buffer }) => Promise<void>
  readLock: (input: { app: string; stage: string }) => Promise<Buffer | null>
  removeLock: (input: { app: string; stage: string }) => Promise<void>
}

/**
 * A place to keep one stage's sealed configuration.
 *
 * `read` returns null when the stage was never written — an empty stage is an
 * answer, not a failure. It throws when a named version is gone, because a
 * caller that asked for a specific revision and silently got the newest is the
 * drift that pinning exists to prevent.
 */
export type StoreBackend = {
  /** Names the cloud this backend keeps configuration in. */
  readonly home: string
  read: (input: { app: string; stage: string; versionId?: string }) => Promise<Buffer | null>
  write: (input: { app: string; stage: string; sealed: Buffer }) => Promise<void>
  /** The revision a deploy should pin, or null when there is nothing to pin. */
  currentVersion: (input: { app: string; stage: string }) => Promise<string | null>
  versions: (input: { app: string; stage: string }) => Promise<StoredVersion[]>
  /** The key this stage's object is sealed with. Never logged, never returned to a caller. */
  passphrase: (input: { app: string; stage: string }) => Promise<Buffer>
  /**
   * The deployment objects the engine of this home leaves for a stage. Which
   * engine, and therefore which layout, is the backend's own business — SST on
   * AWS, Pulumi on GCP.
   */
  readonly state: StateObjects
}

/**
 * Go writes `nonce || ciphertext || tag`, and picks the cipher from the key's
 * length exactly as `aes.NewCipher` does. Kept identical on both backends so a
 * store written by one is readable by the other — which is what makes moving a
 * stage between clouds a copy rather than a re-entry.
 */
const cipherFor = (key: Buffer, where: string): string => {
  const algorithm = { 16: 'aes-128-gcm', 24: 'aes-192-gcm', 32: 'aes-256-gcm' }[key.length]
  if (!algorithm) throw new EnvError(`the passphrase for ${where} is ${key.length} bytes, which is not an AES key size`)
  return algorithm
}

export const seal = (plaintext: string, key: Buffer, where: string): Buffer => {
  const nonce = randomBytes(GCM_NONCE_BYTES)
  const cipher = createCipheriv(cipherFor(key, where) as 'aes-256-gcm', key, nonce)
  const body = Buffer.concat([cipher.update(plaintext, 'utf8'), cipher.final()])
  return Buffer.concat([nonce, body, cipher.getAuthTag()])
}

export const open = (payload: Buffer, key: Buffer, where: string): string => {
  const algorithm = cipherFor(key, where)
  if (payload.length < GCM_NONCE_BYTES + GCM_TAG_BYTES) throw new EnvError(`${where} is too short to be encrypted`)

  const nonce = payload.subarray(0, GCM_NONCE_BYTES)
  const tag = payload.subarray(payload.length - GCM_TAG_BYTES)
  const body = payload.subarray(GCM_NONCE_BYTES, payload.length - GCM_TAG_BYTES)
  const decipher = createDecipheriv(algorithm as 'aes-256-gcm', key, nonce)
  decipher.setAuthTag(tag)
  try {
    return Buffer.concat([decipher.update(body), decipher.final()]).toString('utf8')
  } catch {
    throw new EnvError(`${where} did not decrypt; the passphrase does not match this object`)
  }
}

/** How an object is named, on every backend. Shared so a copy between them is a copy. */
export const objectKey = (app: string, stage: string): string => `secret/${app}/${stage}.json`
