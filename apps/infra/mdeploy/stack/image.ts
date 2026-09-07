/*
 * Where a deploy gets its image addresses.
 *
 * mbuild owns this: it decides the repository, publishes into it, and knows how
 * each registry composes an address. A deploy has one thing to say — which
 * commit — and mbuild answers with the addresses for that commit. Nothing here
 * concatenates a registry host, a repository and a tag, which is exactly the
 * duplication that had `apps/infra/artifacts/api.ts`, the deploy workflow's
 * shell and the stage bootstrap template each spelling out the same ECR name
 * and each able to stop agreeing.
 *
 * This module is only the seam. It converts the deploy's coordinates into the
 * registry mbuild describes, so that the stack modules take an address and stay
 * ignorant of registries entirely.
 */

import { addressesFor, resolveRegistry } from 'mbuild/address'
import { loadBuildConfig, type BuildConfig } from 'mbuild/config'

export type ImagesRequest = {
  /** The commit being deployed. mbuild refuses anything but a full SHA. */
  tag: string
}

/** Artifact name to the address its runtime pulls from. */
export type Images = Record<string, $util.Output<string>>

/**
 * AWS resolves the account only at deploy time, so the registry — and every
 * address built from it — is an Output. Callers pass them straight into a
 * container without ever seeing a registry host.
 */
export const awsImages =
  ({ stage, region, config = loadBuildConfig() }: { stage: string; region: string; config?: BuildConfig }) =>
  (request: ImagesRequest): Images => {
    const accountId = aws.getCallerIdentityOutput({}).accountId
    return Object.fromEntries(
      Object.keys(config.artifacts).map((artifact) => [
        artifact,
        accountId.apply(
          (account: string) =>
            addressesFor({
              config,
              registry: resolveRegistry({ config, stage, region, accountId: account }),
              tag: request.tag,
            })[artifact]!,
        ),
      ]),
    )
  }

/** The project is known before the deploy starts, so nothing here is deferred. */
export const gcpImages =
  ({
    stage,
    region,
    project,
    config = loadBuildConfig(),
  }: {
    stage: string
    region: string
    project: string
    config?: BuildConfig
  }) =>
  (request: ImagesRequest): Images =>
    Object.fromEntries(
      Object.entries(
        addressesFor({ config, registry: resolveRegistry({ config, stage, region, project }), tag: request.tag }),
      ).map(([artifact, address]) => [artifact, $util.output(address)]),
    )
