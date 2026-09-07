// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

import assert from 'node:assert/strict'
import test from 'node:test'

import { parseAssumedRoleName, policyDocumentsAllow, verifyDeployRoleGrantsBoundaryPermission } from './role-boundary.js'

const ACCOUNT_ID = '123456789012'
// The stage the fixtures below are scoped to, matching the verifyDeployRoleGrantsBoundaryPermission
// calls at the bottom of this file. The real document writes <STAGE> here.
const STAGE = 'dev'
const CALLER_ARN = `arn:aws:sts::${ACCOUNT_ID}:assumed-role/boxlite-app-dev-deploy/deploy-dev-stack-30606029374`

// Mirrors bootstrap/aws/deploy-role-policy.json's inline policy verbatim
// (Sid: SetBoxLiteRoleBoundary), so a change to the real document that this
// check can no longer see is caught by editing this fixture, not by a live
// AWS surprise.
function boundedRoleStatements(boundaryArn: any) {
  return [
    {
      Sid: 'ReadIamAndAccountMetadata',
      Effect: 'Allow',
      Action: ['iam:GetRole', 'iam:ListRolePolicies', 'sts:GetCallerIdentity'],
      Resource: '*',
    },
    {
      Sid: 'ManageBoxLiteRoles',
      Effect: 'Allow',
      Action: ['iam:AttachRolePolicy', 'iam:UpdateRole'],
      Resource: [`arn:aws:iam::${ACCOUNT_ID}:role/boxlite-${STAGE}-*`],
    },
    {
      Sid: 'SetBoxLiteRoleBoundary',
      Effect: 'Allow',
      Action: 'iam:PutRolePermissionsBoundary',
      Resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-${STAGE}-*`,
      Condition: { StringEquals: { 'iam:PermissionsBoundary': boundaryArn } },
    },
  ]
}

test('parseAssumedRoleName extracts the role name from an assumed-role ARN', () => {
  assert.equal(parseAssumedRoleName(CALLER_ARN), 'boxlite-app-dev-deploy')
})

test('parseAssumedRoleName rejects a non-assumed-role ARN', () => {
  assert.throws(
    () => parseAssumedRoleName(`arn:aws:iam::${ACCOUNT_ID}:role/boxlite-app-dev-deploy`),
    /is not an assumed-role ARN/,
  )
})

test('policyDocumentsAllow matches the real SetBoxLiteRoleBoundary statement', () => {
  const boundaryArn = `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-dev-runtime-boundary`
  const allowed = policyDocumentsAllow([{ Statement: boundedRoleStatements(boundaryArn) }], {
    action: 'iam:PutRolePermissionsBoundary',
    resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-dev-verify-probe`,
    conditionKey: 'iam:PermissionsBoundary',
    conditionValue: boundaryArn,
  })
  assert.equal(allowed, true)
})

test('policyDocumentsAllow rejects when the condition value is for a different stage', () => {
  const devBoundaryArn = `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-dev-runtime-boundary`
  const productionBoundaryArn = `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-production-runtime-boundary`
  const allowed = policyDocumentsAllow([{ Statement: boundedRoleStatements(devBoundaryArn) }], {
    action: 'iam:PutRolePermissionsBoundary',
    // This stage's role, deliberately: policyDocumentsAllow returns on the first mismatch, so a
    // production-named resource would be rejected by the Resource check and never reach the
    // condition comparison this test is named for. That is what a stage-scoped fixture made it do.
    resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-${STAGE}-verify-probe`,
    conditionKey: 'iam:PermissionsBoundary',
    conditionValue: productionBoundaryArn,
  })
  assert.equal(allowed, false)
})

test('policyDocumentsAllow reproduces today\'s incident: policy has no boundary-set statement at all', () => {
  const boundaryArn = `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-dev-runtime-boundary`
  const [readOnly, manageRoles] = boundedRoleStatements(boundaryArn) // drop SetBoxLiteRoleBoundary
  const allowed = policyDocumentsAllow([{ Statement: [readOnly, manageRoles] }], {
    action: 'iam:PutRolePermissionsBoundary',
    resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-dev-verify-probe`,
    conditionKey: 'iam:PermissionsBoundary',
    conditionValue: boundaryArn,
  })
  assert.equal(allowed, false)
})

test('policyDocumentsAllow treats an unconditional allow as sufficient', () => {
  const allowed = policyDocumentsAllow(
    [
      {
        Statement: [
          { Effect: 'Allow', Action: 'iam:PutRolePermissionsBoundary', Resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-*` },
        ],
      },
    ],
    {
      action: 'iam:PutRolePermissionsBoundary',
      resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-dev-verify-probe`,
      conditionKey: 'iam:PermissionsBoundary',
      conditionValue: `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-dev-runtime-boundary`,
    },
  )
  assert.equal(allowed, true)
})

test('policyDocumentsAllow ignores a Deny statement instead of treating it as a grant', () => {
  const boundaryArn = `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-dev-runtime-boundary`
  const allowed = policyDocumentsAllow(
    [
      {
        Statement: {
          Effect: 'Deny',
          Action: 'iam:PutRolePermissionsBoundary',
          Resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-*`,
        },
      },
    ],
    {
      action: 'iam:PutRolePermissionsBoundary',
      resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-dev-verify-probe`,
      conditionKey: 'iam:PermissionsBoundary',
      conditionValue: boundaryArn,
    },
  )
  assert.equal(allowed, false)
})

test('policyDocumentsAllow finds a grant in a second (e.g. managed) policy document', () => {
  const boundaryArn = `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-dev-runtime-boundary`
  const inlineOnlyReadAccess = { Statement: [{ Effect: 'Allow', Action: 'iam:GetRole', Resource: '*' }] }
  const managedGrantsBoundary = { Statement: boundedRoleStatements(boundaryArn) }
  const allowed = policyDocumentsAllow([inlineOnlyReadAccess, managedGrantsBoundary], {
    action: 'iam:PutRolePermissionsBoundary',
    resource: `arn:aws:iam::${ACCOUNT_ID}:role/boxlite-dev-verify-probe`,
    conditionKey: 'iam:PermissionsBoundary',
    conditionValue: boundaryArn,
  })
  assert.equal(allowed, true)
})

test('verifyDeployRoleGrantsBoundaryPermission ties the caller ARN, account, and stage together', () => {
  const boundaryArn = `arn:aws:iam::${ACCOUNT_ID}:policy/boxlite-dev-runtime-boundary`
  const result = verifyDeployRoleGrantsBoundaryPermission({
    callerArn: CALLER_ARN,
    accountId: ACCOUNT_ID,
    stage: 'dev',
    policyDocuments: [{ Statement: boundedRoleStatements(boundaryArn) }],
  })
  assert.deepEqual(result, { roleName: 'boxlite-app-dev-deploy', boundaryArn, grants: true })
})

test('verifyDeployRoleGrantsBoundaryPermission reports false for the actual failed run (2026-07-31)', () => {
  // The exact shape of https://github.com/boxlite-ai/boxlite/actions/runs/30606029374/job/91078321370:
  // boxlite-app-dev-deploy has no statement granting iam:PutRolePermissionsBoundary at all.
  const result = verifyDeployRoleGrantsBoundaryPermission({
    callerArn: CALLER_ARN,
    accountId: ACCOUNT_ID,
    stage: 'dev',
    policyDocuments: [
      { Statement: [{ Effect: 'Allow', Action: 'iam:GetRole', Resource: '*' }] },
    ],
  })
  assert.equal(result.grants, false)
})
