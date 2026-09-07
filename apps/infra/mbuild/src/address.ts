/*
 * Where an image lives, so nothing else has to build the string.
 *
 * This is the module mdeploy imports. A deploy names a component and a commit;
 * what comes back is the address that cloud's runtime will pull from. Neither
 * side concatenates anything, which is the point — the two registries put the
 * component in different halves of the address:
 *
 *   ECR                <registry>/<repository>:<tag>-<component>
 *   Artifact Registry  <host>/<project>/<repository>/<component>:<tag>
 *
 * A caller that built its own string would work against one registry and
 * quietly produce a nonsense address for the other. `sst.config.ts:292-294` and
 * `publish-image.yml:21,89` are two such strings today, and they already
 * disagree about whether the repository name carries the stage.
 *
 * Pure: no network, no credentials. The registry's own coordinates — an account
 * id, a project — are inputs, because whoever calls this already resolved an
 * identity and asking again would be a second source of truth.
 */

import { registryFor, type BuildConfig } from './config.ts'

/** The tag is a full lowercase commit SHA, so a deploy names exact bytes. */
export const IMAGE_TAG = /^[0-9a-f]{40}$/

/**
 * Where artifacts go, resolved: everything needed to write an address and
 * nothing that has to be looked up again.
 *
 * The declared half — kind, repository — comes from mbuild's config; the region
 * and the account come from mstage, which is where a stage's coordinates are
 * declared once. Asking a second source for either would be one more thing to
 * keep in step. Carrying them here rather than reaching back into a config is
 * what lets one process address two stages at once, which is exactly what
 * promoting an image between them is.
 */
export type Registry =
  | {
      kind: 'ecr'
      /** `<account>.dkr.ecr.<region>.amazonaws.com`. */
      host: string
      repository: string
      /** Kept because every ECR API call needs it, not only the host. */
      region: string
    }
  | {
      kind: 'artifact-registry'
      /** `<region>-docker.pkg.dev`. */
      host: string
      project: string
      repository: string
    }

export class ImageAddressError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ImageAddressError'
  }
}

export const assertTag = (tag: string): string => {
  if (!IMAGE_TAG.test(tag)) {
    throw new ImageAddressError(`An image tag must be one full lowercase commit SHA; got ${JSON.stringify(tag)}`)
  }
  return tag
}

const assertArtifact = (config: BuildConfig, artifact: string): string => {
  if (!(artifact in config.artifacts)) {
    const known = Object.keys(config.artifacts).join(', ')
    throw new ImageAddressError(`${config.path} declares no artifact "${artifact}". Declared: ${known}`)
  }
  return artifact
}

/**
 * The address for one artifact at one commit.
 *
 * On ECR the artifact is a tag suffix, because one repository holds every
 * artifact; on Artifact Registry it is a path segment, because a repository
 * there is a namespace rather than a single image.
 */
export const addressFor = ({
  config,
  registry,
  artifact,
  tag,
}: {
  config: BuildConfig
  registry: Registry
  artifact: string
  tag: string
}): string => {
  assertArtifact(config, artifact)
  assertTag(tag)
  switch (registry.kind) {
    case 'ecr':
      return `${registry.host}/${registry.repository}:${tag}-${artifact}`
    case 'artifact-registry':
      return `${registry.host}/${registry.project}/${registry.repository}/${artifact}:${tag}`
  }
}

/** Every artifact at one commit, which is what a publish iterates over. */
export const addressesFor = ({
  config,
  registry,
  tag,
}: {
  config: BuildConfig
  registry: Registry
  tag: string
}): Record<string, string> =>
  Object.fromEntries(
    Object.keys(config.artifacts).map((artifact) => [artifact, addressFor({ config, registry, artifact, tag })]),
  )

/**
 * The ECR host for an account and region. Named here rather than at the call
 * sites so the one place that knows the ECR address shape is this file.
 */
export const ecrHost = ({ accountId, region }: { accountId: string; region: string }): string => {
  if (!/^\d{12}$/.test(accountId)) throw new ImageAddressError(`An AWS account id is twelve digits; got ${accountId}`)
  return `${accountId}.dkr.ecr.${region}.amazonaws.com`
}

/** The Artifact Registry host for a region. */
export const artifactRegistryHost = (region: string): string => `${region}-docker.pkg.dev`

/**
 * One stage's registry, resolved: the declared half from the config and the
 * account or project from the caller's identity.
 */
export const resolveRegistry = ({
  config,
  stage,
  region,
  accountId,
  project,
}: {
  config: BuildConfig
  stage: string
  /** Where the stage lives. mstage declares this; mbuild does not repeat it. */
  region: string
  /** Required for ECR. */
  accountId?: string
  /** Required for Artifact Registry. */
  project?: string
}): Registry => {
  const declared = registryFor(config, stage)
  if (!region.trim()) throw new ImageAddressError(`Stage "${stage}" has no region; mstage declares where a stage lives`)
  switch (declared.kind) {
    case 'ecr':
      if (!accountId) throw new ImageAddressError(`Stage "${stage}" publishes to ECR, which needs an account id`)
      return {
        kind: 'ecr',
        host: ecrHost({ accountId, region }),
        repository: declared.repository,
        region,
      }
    case 'artifact-registry':
      if (!project) throw new ImageAddressError(`Stage "${stage}" publishes to Artifact Registry, which needs a project`)
      return {
        kind: 'artifact-registry',
        host: artifactRegistryHost(region),
        project,
        repository: declared.repository,
      }
  }
}
