/**
 * A fingerprint of the configuration a deploy will run with.
 *
 * The point is to catch a store that changed after something was built against
 * it: whoever consumes the configuration recomputes this and compares. That only
 * works if both sides compute it identically, so the formula is copied from the
 * one already in use — `apps/api/src/sst-environment.store.ts:49-57` — and must
 * not be "improved" independently of it.
 *
 * Each entry contributes `<len>:<key>=<len>:<value>\n` over keys in sorted
 * order. The lengths are what stop `A=1,B=` and `A=1,B` from colliding.
 */

import { createHash } from 'node:crypto'

export type Digest = { key: string; group: string }

export const digestOf = (values: Record<string, string>): string => {
  const hash = createHash('sha256')
  for (const key of Object.keys(values).sort()) {
    const value = values[key] ?? ''
    hash.update(`${key.length}:${key}=${value.length}:${value}\n`)
  }
  return hash.digest('hex')
}

/**
 * The digest of a group, over everything in it but the key holding the digest.
 * Including it would make the value depend on itself.
 */
export const digestOfGroup = ({ values, digestKey }: { values: Record<string, string>; digestKey: string }): string =>
  digestOf(Object.fromEntries(Object.entries(values).filter(([key]) => key !== digestKey)))

export type DigestComparison = {
  expected: string
  stored: string | null
  matches: boolean
}

export const compareDigest = ({
  values,
  digestKey,
}: {
  values: Record<string, string>
  digestKey: string
}): DigestComparison => {
  const expected = digestOfGroup({ values, digestKey })
  const stored = values[digestKey] ?? null
  return { expected, stored, matches: stored === expected }
}
