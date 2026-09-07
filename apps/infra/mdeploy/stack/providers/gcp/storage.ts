/*
 * The control plane's own bucket on Cloud Storage, and the grants that bound
 * what it may do to the volume buckets it creates.
 *
 * The shape is the AWS module's, and one thing inside it is genuinely
 * different. AWS can write a grant against `arn:aws:s3:::<prefix>-*` — a
 * wildcard over buckets that do not exist yet. Google's IAM has no wildcard
 * over resource names: a binding names one bucket, or a role is granted at the
 * project. So the lifecycle grant here is two project-level roles rather than a
 * prefix-scoped policy, and the prefix stops being enforced by IAM.
 *
 * That is a real widening and it is not hidden. What holds the line instead is
 * a *condition* on the binding: an IAM condition on the resource name,
 * expressed in CEL, which Google does support. It is the same sentence as the
 * ARN prefix, written the way this cloud writes it — and a stage whose
 * condition failed to apply would be a stage where the API could delete any
 * bucket in the project, which is why it is a condition rather than a comment.
 */

import type { Storage, StorageProvider, StorageRequest } from '../../storage.ts'

/** What the API calls against a volume bucket. The mirror of the AWS list. */
const VOLUME_ROLE = 'roles/storage.admin'

/**
 * What a vended credential may do inside one volume bucket.
 *
 * The ceiling, not the grant: the API's provider attaches it to the vending
 * account under the same prefix condition, and the per-organization condition
 * the API adds when it mints a token narrows it further. Effective access is
 * the intersection, exactly as on AWS.
 */
export const VOLUME_OBJECT_ACCESS_ROLE = 'roles/storage.objectAdmin'

/**
 * The account the API mints a per-organization token for.
 *
 * Google's equivalent of assuming a role: the API holds
 * `iam.serviceAccountTokenCreator` on this account and mints a short-lived
 * access token scoped to one organization's prefix. The account is created
 * here, beside the bucket it is about, for the same reason the AWS role's
 * *name* is declared here — the two halves of one idea belong together.
 */
export const gcpStorageProvider =
  ({ project, region }: { project: string; region: string }): StorageProvider =>
  (request: StorageRequest): Storage => {
    const prefix = `${$app.name}-${$app.stage}`

    const bucket = new gcp.storage.Bucket('Storage', {
      name: `${prefix}-storage`,
      project,
      location: region.toUpperCase(),
      uniformBucketLevelAccess: true,
      // The only guard against an object-level overwrite, which no removal
      // policy covers.
      versioning: { enabled: request.versioning },
      // Refuse a public binding outright rather than relying on nobody adding
      // one: a volume bucket made public is the failure this whole module's
      // grants are shaped around.
      publicAccessPrevention: 'enforced',
    })

    const vending = new gcp.serviceaccount.Account('VolumeAccessAccount', {
      project,
      accountId: `${prefix}-volume`.slice(0, 30),
      displayName: `BoxLite volume access (${$app.stage})`,
    })

    /*
     * The CEL that says "buckets under this prefix, and no others".
     *
     * `resource.name` for a bucket is `projects/_/buckets/<name>`, so the
     * condition is written against that shape rather than against the bare
     * name. Google evaluates a false condition as no grant at all, which is the
     * behaviour wanted: a malformed prefix denies rather than widens.
     */
    const volumeCondition = {
      title: 'volume-buckets-only',
      description: `Buckets named ${request.volumePrefix}-*`,
      expression: `resource.name.startsWith("projects/_/buckets/${request.volumePrefix}-")`,
    }

    return {
      name: bucket.name,
      binding: {
        cloud: 'gcp',
        bucketUrl: bucket.url,
        /*
         * List-only against its own bucket. Granted as a binding on that one
         * bucket by the API's provider, which is where every grant on this
         * cloud is attached — a bucket-level binding needs no condition,
         * because it already names exactly one resource.
         */
        listGrant: 'roles/storage.legacyBucketReader',
        lifecycleGrant: VOLUME_ROLE,
        volumeCondition,
        credentialVending: { serviceAccount: vending.email },
      },
      ready: [],
    }
  }
