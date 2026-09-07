// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

/*
 * Pure IAM-policy-document checks for deployment/verify-role.ts — a
 * CI preflight that catches "bootstrap was never (re)run for this stage" in
 * seconds, using only the read-only IAM permissions already granted to the
 * deploy role (the ReadIamAndAccountMetadata statement in
 * bootstrap/aws/deploy-role-policy.json), instead of the ~2-minute wall of
 * duplicate iam:PutRolePermissionsBoundary AccessDenied errors a real
 * `sst deploy` produces when this is missing.
 *
 * This does not reimplement AWS's full policy evaluation (explicit Deny,
 * SCPs, permission boundaries on the deploy role itself, and so on) — it
 * only answers "is there an Allow statement a reasonable operator would
 * expect to grant this call", which is enough to catch the missing-bootstrap
 * case fast without becoming a policy simulator. A false pass here still
 * fails safely: the real deploy step re-checks it for real.
 */

import { runtimeBoundaryPolicyArn } from './environment.js'

const ASSUMED_ROLE_ARN_PATTERN = /^arn:aws:sts::\d{12}:assumed-role\/([^/]+)\/[^/]+$/

export function parseAssumedRoleName(arn: any) {
  const match = typeof arn === 'string' ? arn.match(ASSUMED_ROLE_ARN_PATTERN) : null
  if (!match) {
    throw new Error(`'${arn}' is not an assumed-role ARN (expected arn:aws:sts::<account>:assumed-role/<role>/<session>)`)
  }
  return match[1]
}

function asArray(value: any) {
  if (value === undefined) return []
  return Array.isArray(value) ? value : [value]
}

function iamPatternToRegExp(pattern: any) {
  if (typeof pattern !== 'string') return /(?!)/ // matches nothing
  const escaped = pattern.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.')
  return new RegExp(`^${escaped}$`, 'i')
}

/*
 * IAM treats condition *key names* case-insensitively, so a policy written with
 * `iam:PermissionsBoundary` must match a lookup for `IAM:PermissionsBoundary`.
 * Only the key is folded — the values `StringEquals` compares stay
 * case-sensitive, which is why the caller still uses an exact `includes`.
 */
function stringEqualsValues(condition: any, conditionKey: any) {
  const stringEquals = condition?.StringEquals
  if (!stringEquals) return undefined
  const wanted = conditionKey.toLowerCase()
  const match = Object.keys(stringEquals).find((key) => key.toLowerCase() === wanted)
  return match === undefined ? undefined : stringEquals[match]
}

/*
 * Recognizes the `Condition.StringEquals` operator the actual document uses
 * (SetBoxLiteRoleBoundary in bootstrap/aws/deploy-role-policy.json). A statement with no
 * condition on `conditionKey` is an unconditional — and therefore sufficient
 * — grant of the action; a policy redesigned around a different condition
 * operator would need this updated too.
 */
export function policyDocumentsAllow(policyDocuments: any, { action, resource, conditionKey, conditionValue }: any) {
  for (const document of policyDocuments) {
    for (const statement of asArray(document?.Statement)) {
      if (statement?.Effect !== 'Allow') continue
      if (!asArray(statement.Action).some((pattern) => iamPatternToRegExp(pattern).test(action))) continue
      if (!asArray(statement.Resource).some((pattern) => iamPatternToRegExp(pattern).test(resource))) continue

      const requiredValues = stringEqualsValues(statement.Condition, conditionKey)
      if (requiredValues === undefined || asArray(requiredValues).includes(conditionValue)) return true
    }
  }
  return false
}

/**
 * @param {{
 *   callerArn: string,        // sts:GetCallerIdentity Arn
 *   accountId: string,        // sts:GetCallerIdentity Account
 *   stage: string,
 *   policyDocuments: object[],// parsed IAM PolicyDocument JSON (inline + managed, current version)
 *   appName?: string,
 * }} args
 */
export function verifyDeployRoleGrantsBoundaryPermission({ callerArn, accountId, stage, policyDocuments, appName = 'boxlite' }: any) {
  const roleName = parseAssumedRoleName(callerArn)
  const boundaryArn = runtimeBoundaryPolicyArn({ accountId, appName, stage })
  // A synthetic-but-representative role ARN under the namespace SST creates
  // roles in — not a real resource, just something the policy's
  // `role/boxlite-<stage>-*`-style Resource pattern should match.
  const probeResource = `arn:aws:iam::${accountId}:role/${appName}-${stage}-verify-probe`

  const grants = policyDocumentsAllow(policyDocuments, {
    action: 'iam:PutRolePermissionsBoundary',
    resource: probeResource,
    conditionKey: 'iam:PermissionsBoundary',
    conditionValue: boundaryArn,
  })
  return { roleName, boundaryArn, grants }
}
