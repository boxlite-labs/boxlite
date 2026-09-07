/*
 * The object store boxes keep their volumes in.
 *
 * BoxLite's storage module is unusual among the modules here: the bucket this
 * one creates is not where box data lives. Volumes get a bucket each, created
 * by the API at runtime, and this is the control plane's own — the one it lists
 * on boot to prove it can reach object storage at all, and the namespace the
 * per-volume buckets are named under.
 *
 * That is why the handle carries two grants rather than one. `listGrant` is
 * what the API needs against *this* bucket, and it is deliberately list-only.
 * `lifecycleGrant` is what it needs against the volume buckets it creates, and
 * is scoped by name prefix rather than by bucket, because the buckets it names
 * do not exist yet when the grant is written. Keeping them apart is what stops
 * the API's own probe from carrying the right to delete a customer's volume.
 */

export type StorageRequest = {
  /**
   * The prefix every volume bucket is named under. The lifecycle grant is
   * written against `<prefix>-*`, so this is a security boundary rather than a
   * naming convention: widening it widens what the API may delete.
   */
  volumePrefix: string
  /** Keep old versions of an object. On everywhere — it is the only guard
   * against an object-level overwrite, which no removal policy covers. */
  versioning: boolean
}

/**
 * The cloud-specific half.
 *
 * `credentialVending` is the one idea with no shared shape at all. On AWS the
 * API assumes a role with a per-organization session policy, and what it needs
 * is that role's name and ARN. On GCP the equivalent is minting a short-lived
 * access token for a service account, and what it needs is that account's
 * email. Neither value means anything to the other cloud's SDK, so the union is
 * the honest description.
 */
export type StorageBinding =
  | {
      cloud: 'aws'
      bucketArn: $util.Output<string>
      listGrant: $util.Output<string>
      lifecycleGrant: $util.Output<string>
      credentialVending: { roleName: string; roleArn: $util.Output<string> }
    }
  | {
      cloud: 'gcp'
      bucketUrl: $util.Output<string>
      /**
       * Role names rather than policy documents, because that is what a grant
       * *is* on this cloud: a binding names a role, a member and a resource,
       * and there is no document to hand a principal.
       */
      listGrant: string
      lifecycleGrant: string
      /**
       * The CEL that bounds the lifecycle grant to the volume prefix.
       *
       * Google's IAM has no wildcard over resource names, so the AWS side's
       * `arn:aws:s3:::<prefix>-*` has no direct counterpart — the role has to be
       * granted at the project. This condition is what puts the prefix back,
       * and it is the whole of the boundary: without it the API could delete
       * any bucket in the project.
       */
      volumeCondition: { title: string; description: string; expression: string }
      credentialVending: { serviceAccount: $util.Output<string> }
    }

export type Storage = {
  /** The bucket's own name, which the API reads as its default bucket. */
  name: $util.Output<string>
  binding: StorageBinding
  ready: any[]
}

export type StorageProvider = (request: StorageRequest) => Storage
