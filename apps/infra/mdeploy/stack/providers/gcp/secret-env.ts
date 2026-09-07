/*
 * How a Cloud Run container is handed a secret by reference.
 *
 * Two shapes, and a container reads both the same way. A plain value becomes an
 * `env` entry with `value`; an address becomes one with `valueSource.
 * secretKeyRef`, which the platform resolves just before the container starts —
 * so the value never enters the revision, and `gcloud run revisions describe`
 * shows a reference rather than a password.
 *
 * A name in both is not a preference the platform resolves: a Cloud Run
 * revision refuses the pair outright, where an ECS task definition would take
 * whichever was written last. The composition root refuses it earlier, but this
 * is where the difference would surface, so it is named here too.
 *
 * `secretKeyRef.secret` takes a secret and `version` takes the version beside
 * it, so a reference carrying `/versions/<v>` in the `secret` field declares the
 * version twice and Cloud Run resolves the pair to nothing. Splitting happens
 * here and only here, which is what keeps the shape from being written out at
 * each call site — the way it goes wrong four times before anyone notices.
 *
 * Two forms arrive, from two sources that genuinely differ:
 *
 *   projects/<p>/secrets/<s>              a stored address, from the `secret`
 *                                         marker group. mstage's own validator
 *                                         *refuses* a version on the end
 *                                         (`env/secret-address.ts`), so this
 *                                         form can never carry one.
 *   projects/<p>/secrets/<s>/versions/<v> minted by a provider in this bundle,
 *                                         which controls the string and pins the
 *                                         version so a rotation is a change the
 *                                         deploy can see.
 *
 * Accepting both rather than picking one: an operator writing into the store
 * cannot pin, and a provider that has just created a version should not throw
 * that away. A stored address resolves `latest`, which is the only thing it
 * could mean.
 */

/** What a reference with no version of its own resolves to. */
const LATEST = 'latest'

/** One `env` entry, in the shape `gcp.cloudrunv2.Service` takes. */
export type ContainerEnv = {
  name: string
  value?: $util.Input<string>
  valueSource?: { secretKeyRef: { secret: $util.Input<string>; version: $util.Input<string> } }
}

/**
 * A Secret Manager reference, split into the two parts Cloud Run wants.
 *
 * Both accepted forms are named above. What it refuses is a string that is not
 * a reference at all — a plaintext secret about to be delivered as if it named
 * one, which is the failure the whole by-reference channel exists to prevent.
 */
export const splitSecretRef = (reference: string): { secret: string; version: string } => {
  const match = /^projects\/[^/]+\/secrets\/([^/]+)(?:\/versions\/([^/]+))?$/.exec(reference)
  if (!match) {
    throw new Error(
      `${JSON.stringify(reference)} is not a Secret Manager reference; a GCP stage delivers a secret as ` +
        'projects/<project>/secrets/<secret>, optionally with /versions/<version> when a provider pinned one',
    )
  }
  return { secret: match[1] as string, version: match[2] ?? LATEST }
}

/**
 * The secret's own id, for a caller that grants access rather than mounts it.
 *
 * The same parse, so a form one of them accepts is a form the other does. Two
 * regexes for one reference is how the granting side ends up refusing an address
 * the mounting side had just accepted.
 */
export const secretIdOf = (reference: string): string => splitSecretRef(reference).secret

/**
 * The container's whole environment: values and addresses, in one list.
 *
 * One list because that is what Cloud Run takes — unlike ECS, which has
 * separate `environment` and `secrets` arrays. The two channels are still
 * distinct in what they carry; they merely arrive in the same array.
 */
export const containerEnvironment = ({
  values,
  addresses,
}: {
  values: Record<string, $util.Input<string>>
  addresses: Record<string, $util.Input<string>>
}): $util.Output<ContainerEnv[]> => {
  const plain: ContainerEnv[] = Object.entries(values).map(([name, value]) => ({ name, value }))
  const names = Object.keys(addresses)
  if (names.length === 0) return $util.output(plain)
  return $resolve(Object.values(addresses)).apply((references: string[]) => [
    ...plain,
    ...names.map((name, index) => {
      const { secret, version } = splitSecretRef(references[index] as string)
      return { name, valueSource: { secretKeyRef: { secret, version } } }
    }),
  ])
}
