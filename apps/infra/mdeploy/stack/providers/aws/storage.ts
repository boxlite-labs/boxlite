/*
 * The control plane's own bucket, and the two grants that bound what it may do
 * to the volume buckets it creates.
 *
 * Neither grant is attached here. Both are policy documents this module hands
 * to the API's provider, which attaches them to the task role — because a
 * bucket cannot grant anything to a principal that does not exist yet, and the
 * API's task role is created with the API.
 *
 * The lifecycle grant is written against `<prefix>-*` rather than against a
 * list of buckets, and that is not a convenience: the buckets it names are
 * created by the API at runtime and do not exist at deploy time. Which makes
 * the prefix a security boundary — widening it widens what a compromised
 * control plane can delete — and is why `mdeploy.config.json` validates it more
 * strictly than either cloud does.
 *
 * The action lists are deliberately not `s3:*`. Every action here is one the
 * API demonstrably calls; the tail that is missing — `PutBucketPolicy`,
 * `PutBucketAcl` and the rest — is exactly what would let a compromised control
 * plane make a customer's volume public.
 */

import type { Storage, StorageProvider, StorageRequest } from '../../storage.ts'

/**
 * What the API calls against a volume bucket, and nothing else.
 *
 * From `volume.manager.ts`'s create and tag path and `delete-s3-bucket.ts`'s
 * empty and delete path. A new S3 call in the application needs a matching
 * action added here, which is the review this list exists to force.
 */
const VOLUME_BUCKET_ACTIONS = [
  's3:CreateBucket',
  's3:DeleteBucket',
  's3:GetBucketLocation',
  's3:GetBucketTagging',
  's3:PutBucketTagging',
  's3:ListBucket',
  's3:ListBucketVersions',
]

const VOLUME_OBJECT_ACTIONS = [
  's3:GetObject',
  's3:PutObject',
  's3:DeleteObject',
  's3:DeleteObjectVersion',
  's3:AbortMultipartUpload',
]

/**
 * The role the API assumes to vend a per-organization credential.
 *
 * The name is declared here and the resource is created by the API's provider,
 * because its trust policy has to name the API's task role — which exists only
 * once the API does. Declaring the name first is what breaks that cycle, and
 * putting it in this module is what keeps the two halves of one idea together.
 */
export const volumeAccessRoleName = (): string => `${$app.name}-${$app.stage}-s3-access`

export const awsStorageProvider =
  ({ accountId }: { accountId: string }): StorageProvider =>
  (request: StorageRequest): Storage => {
    const bucket = new sst.aws.Bucket('Storage', { versioning: request.versioning })
    const roleName = volumeAccessRoleName()
    const roleArn = $interpolate`arn:aws:iam::${accountId}:role/${roleName}`

    return {
      name: bucket.name,
      binding: {
        cloud: 'aws',
        bucketArn: bucket.arn,
        /*
         * List-only against its own bucket. The API's boot probe asks whether
         * object storage is reachable; it never reads an object out of this
         * one, so nothing here grants it.
         */
        listGrant: bucket.arn.apply((arn: string) =>
          JSON.stringify({
            Version: '2012-10-17',
            Statement: [{ Effect: 'Allow', Action: ['s3:ListBucket'], Resource: [arn] }],
          }),
        ),
        lifecycleGrant: $util.output(request.volumePrefix).apply((prefix: string) =>
          JSON.stringify({
            Version: '2012-10-17',
            Statement: [
              { Effect: 'Allow', Action: VOLUME_BUCKET_ACTIONS, Resource: [`arn:aws:s3:::${prefix}-*`] },
              { Effect: 'Allow', Action: VOLUME_OBJECT_ACTIONS, Resource: [`arn:aws:s3:::${prefix}-*/*`] },
            ],
          }),
        ),
        credentialVending: { roleName, roleArn },
      },
      ready: [],
    }
  }
