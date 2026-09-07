/*
 * Where a stage's registry lives, in the words that kind of registry uses.
 *
 * ECR is addressed by account and Artifact Registry by project, and the two are
 * not interchangeable. The account comes from the caller's own credentials: an
 * ECR address is derived from the account the push is authorised in, and
 * reading it from the config instead would let the address and the
 * authorisation disagree. A project cannot be read that way — nothing here
 * holds a Google identity — so it comes from the one file that declares where a
 * stage lives.
 *
 * In `src/` rather than in `bin/` because it decides something. The bin is
 * wiring: it parses argv and prints. A rule that can be wrong belongs where a
 * test can reach it.
 */

import type { RegistryConfig } from './config.ts'

export class CoordinatesError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'CoordinatesError'
  }
}

/** What `resolveRegistry` needs beyond the stage's own declaration. */
export type Coordinates = { accountId: string } | { project: string }

export type CoordinatesInput = {
  stage: string
  kind: RegistryConfig['kind']
  /** The project the stage declares, from `mstage.config.json`. Null on AWS. */
  project: string | null
  /** The account the credentials in hand belong to. Asked only when ECR needs it. */
  accountId: () => Promise<string>
}

export const coordinatesOf = async ({ stage, kind, project, accountId }: CoordinatesInput): Promise<Coordinates> => {
  if (kind === 'ecr') return { accountId: await accountId() }
  if (!project) {
    throw new CoordinatesError(`mstage.config.json gives stage "${stage}" no project, which is where it publishes to`)
  }
  return { project }
}

/**
 * Whether one image can be promoted from one stage to the other.
 *
 * A promotion copies between two registries with one identity, and no identity
 * holds both clouds. Refused rather than half-attempted: the pull would
 * authenticate and the push would not, partway through a set of artifacts.
 */
export const assertPromotable = ({
  from,
  to,
}: {
  from: { stage: string; kind: RegistryConfig['kind'] }
  to: { stage: string; kind: RegistryConfig['kind'] }
}): void => {
  if (from.kind === to.kind) return
  throw new CoordinatesError(
    `Cannot promote ${from.stage} (${from.kind}) to ${to.stage} (${to.kind}): ` +
      'a promotion copies within one registry kind, using one identity',
  )
}
