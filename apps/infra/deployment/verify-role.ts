// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

/*
 * CI preflight: confirm the assumed deploy role can actually attach the
 * runtime IAM permissions boundary before `sst diff`/`sst deploy` run.
 * apps/infra/stack/deploy.ts requires every role it manages to carry that
 * boundary (see the $transform there); if bootstrap/aws.ts (driven by
 * bootstrap/bootstrap.ts) was never (re)run for this stage, every one of
 * those roles fails identically with an iam:PutRolePermissionsBoundary
 * AccessDenied — a ~2-minute wall of duplicate errors, discovered only after
 * install + tests + preview already ran. This step catches the same gap in
 * seconds, using only the read-only IAM actions the deploy role already has
 * (ReadIamAndAccountMetadata in bootstrap/aws/deploy-role-policy.json).
 *
 * Usage: npm run verify-deploy-role
 * Reads the SST stage from IAM_PERMISSIONS_BOUNDARY_STAGE (already required
 * in the deploy workflow's job-level env, and by stack/deploy.ts itself).
 */

import { parseAssumedRoleName, verifyDeployRoleGrantsBoundaryPermission } from './role-boundary.js'
import { loadDeploymentEnvironment, resolveAwsRegion } from './environment.js'
import { resolveAwsCliPath, runAwsJson } from '../shared/exec.js'

const SCRIPT_NAME = 'verify-deploy-role-boundary'

function requireStage(environment = process.env) {
  const stage = environment.IAM_PERMISSIONS_BOUNDARY_STAGE
  if (!stage) throw new Error('IAM_PERMISSIONS_BOUNDARY_STAGE is required to identify the provisioned runtime boundary')
  return stage
}

function fetchInlinePolicyDocuments(queryAws: any, roleName: any) {
  const { PolicyNames } = queryAws(['iam', 'list-role-policies', '--role-name', roleName])
  return PolicyNames.map(
    (policyName: any) =>
      queryAws(['iam', 'get-role-policy', '--role-name', roleName, '--policy-name', policyName]).PolicyDocument,
  )
}

function fetchAttachedManagedPolicyDocuments(queryAws: any, roleName: any) {
  const { AttachedPolicies } = queryAws(['iam', 'list-attached-role-policies', '--role-name', roleName])
  return AttachedPolicies.map(({ PolicyArn }: any) => {
    const { Policy } = queryAws(['iam', 'get-policy', '--policy-arn', PolicyArn])
    const { PolicyVersion } = queryAws([
      'iam',
      'get-policy-version',
      '--policy-arn',
      PolicyArn,
      '--version-id',
      Policy.DefaultVersionId,
    ])
    return PolicyVersion.Document
  })
}

function main() {
  // CI supplies these as job env, but `npm run verify-deploy-role` locally
  // needs the stage dotenv.
  loadDeploymentEnvironment()
  const region = resolveAwsRegion()
  const stage = requireStage()
  const awsCliPath = resolveAwsCliPath()
  const queryAws = (args: any) => runAwsJson(args, { awsCliPath, region })

  let identity
  try {
    identity = queryAws(['sts', 'get-caller-identity'])
  } catch (cause) {
    throw new Error('could not call `aws sts get-caller-identity`', { cause })
  }

  let policyDocuments
  try {
    const roleName = parseAssumedRoleName(identity.Arn)
    policyDocuments = [
      ...fetchInlinePolicyDocuments(queryAws, roleName),
      ...fetchAttachedManagedPolicyDocuments(queryAws, roleName),
    ]
  } catch (cause) {
    throw new Error(`could not read the deploy role's IAM policies for stage '${stage}'`, { cause })
  }

  const { roleName, boundaryArn, grants } = verifyDeployRoleGrantsBoundaryPermission({
    callerArn: identity.Arn,
    accountId: identity.Account,
    stage,
    policyDocuments,
  })

  if (!grants) {
    throw new Error(
      `deploy role '${roleName}' has no policy statement allowing iam:PutRolePermissionsBoundary for ` +
        `${boundaryArn} on role/boxlite-<stage>-*. apps/infra/stack/deploy.ts requires every SST-managed role to carry ` +
        `this boundary. Run \`npm run bootstrap -- --stage ${stage}\` with AWS admin ` +
        'credentials (it reconciles the role against bootstrap/aws/deploy-role-policy.json), then confirm the GitHub environment variable ' +
        `AWS_ACCOUNT_ID for '${stage}' still matches the account that role was created in. ` +
        'See apps/infra/README.md#deploy-an-existing-stack.',
    )
  }

  console.log(`[${SCRIPT_NAME}] ${roleName} grants iam:PutRolePermissionsBoundary for ${boundaryArn}`)
}

try {
  main()
} catch (error: any) {
  // Print the cause chain: failures here are wrapped with `{ cause }`, and the
  // wrapper text alone ("could not read the deploy role's IAM policies") does
  // not say whether the ARN was the wrong shape or the AWS CLI itself failed.
  console.error(`${SCRIPT_NAME}: ${error.message}`)
  for (let cause = error.cause; cause; cause = cause.cause) {
    console.error(`${SCRIPT_NAME}:   caused by: ${cause.message ?? cause}`)
  }
  process.exit(1)
}
