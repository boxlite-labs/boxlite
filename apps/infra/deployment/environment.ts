// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 BoxLite AI

import { config as loadDotenv } from 'dotenv'
import { readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

// Exported because the deploy workflows carry it as a literal: `configure-aws-credentials` needs a
// region before any AWS access exists, so it cannot come from the stage's secret store. A contract
// test pins the YAML against this constant so the two cannot drift.
export const DEFAULT_AWS_REGION = 'ap-southeast-1'

/*
 * The SST app name. One definition, because it is load-bearing in three unrelated places: the app
 * SST deploys (stack/app.ts), the `<app>/<stage>` section header `sst secret list` prints
 * (deployment/stage-config.ts), and the artifact resource names the preflights verify. SST derives
 * every resource name from it, so the three must never disagree.
 */
export const SST_APP_NAME = 'boxlite'
const STABLE_SEMVER_PATTERN = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/
const DEFAULT_MODULE_DIRECTORY = dirname(fileURLToPath(import.meta.url))

/*
 * How the Api image repository and the Runner artifacts bucket are spelled:
 *
 *   <app>-<workload>-<stage>-<name>[-<attribute>...]
 *
 * The workload slot exists because the account holds more than one — the application stack and the
 * e2e fleet — and without it nothing distinguishes a workload token from a stage: `boxlite-e2e-runner`
 * reads equally as stage=e2e or workload=e2e.
 *
 * bootstrap/aws.ts creates both resources by calling apiImageRepository/runnerArtifactsBucketName
 * directly, so it shares this one spelling rather than restating it. Two places still cannot call
 * this and have to re-spell the name instead, which is why a contract test pins each against these
 * parts: deploy-infra.yml writes the bucket into a shell variable, and build-apps-api-image.yml
 * writes the image name — neither is JS. A third is easy to miss and is not a spelling at all — the
 * runtime permissions boundary (bootstrap/aws/runtime-boundary-policy.json) allows S3 by ARN prefix,
 * and a boundary intersects with every identity policy, so renaming the bucket without widening the
 * prefix denies the Runner its own binary while every test stays green.
 *
 * Deliberately not applied to boxlite-<stage>-github-deploy or boxlite-<stage>-runtime-boundary: both
 * are live, referenced by ARN from the deploy workflows and attached to existing roles, so renaming
 * either is a migration rather than an edit. Nor to SST-managed resources, whose names SST derives
 * from the app name itself — moving those means renaming the app and replacing everything it manages.
 */
const WORKLOAD = 'app'

export function awsResourceName({ app, stage, name, attributes = [] }: any) {
  return [app, WORKLOAD, stage, name, ...attributes].join('-')
}

export function loadDeploymentEnvironment({ path, environment = process.env }: { path?: string; environment?: NodeJS.ProcessEnv } = {}) {
  return loadDotenv({
    ...(path ? { path } : {}),
    processEnv: environment,
    quiet: true,
  })
}

export function resolveAwsRegion(environment = process.env) {
  return environment.AWS_REGION?.trim() || DEFAULT_AWS_REGION
}

// The fixed `boxlite-app-<stage>-artifacts-<account>` bucket is the strictest generated AWS name:
// its 35 non-stage characters leave 28 of S3's 63.
const SST_STAGE_PATTERN = /^[a-z0-9][a-z0-9-]{0,27}$/

function validateSstStage(stage: unknown): string {
  if (typeof stage !== 'string' || !SST_STAGE_PATTERN.test(stage)) {
    throw new Error(`invalid SST stage '${String(stage)}' (expected 1-28 lowercase letters, numbers, or hyphens)`)
  }
  return stage
}

export function resolveSstStage(args: readonly string[], environment: NodeJS.ProcessEnv = process.env): string {
  let configuredStage

  for (let index = 0; index < args.length; index++) {
    if (args[index] === '--stage') {
      const stage = args[index + 1]
      if (!stage || stage.startsWith('-')) throw new Error('--stage requires a value')
      if (configuredStage !== undefined) throw new Error('--stage may be specified only once')
      configuredStage = stage
      index += 1
      continue
    }

    const inlineStage = args[index].match(/^--stage=(.*)$/)?.[1]
    if (inlineStage !== undefined) {
      if (!inlineStage) throw new Error('--stage requires a value')
      if (configuredStage !== undefined) throw new Error('--stage may be specified only once')
      configuredStage = inlineStage
    }
  }

  if (configuredStage !== undefined) return validateSstStage(configuredStage)
  if (environment.SST_STAGE !== undefined && environment.SST_STAGE !== '') {
    return validateSstStage(environment.SST_STAGE)
  }
  // Never let a mutating command fall back to the personal default stage.
  if (args[0] === 'deploy' || args[0] === 'remove') {
    throw new Error(`${args[0]} requires an explicit --stage or SST_STAGE`)
  }
  return 'dev'
}

export function requireIamPermissionsBoundaryStage(
  stage: unknown,
  environment: NodeJS.ProcessEnv = process.env,
): string {
  const selectedStage = validateSstStage(stage)
  const configuredStage = environment.IAM_PERMISSIONS_BOUNDARY_STAGE
  if (!configuredStage) {
    throw new Error('IAM_PERMISSIONS_BOUNDARY_STAGE is required to identify the provisioned runtime boundary')
  }

  const boundaryStage = validateSstStage(configuredStage)
  if (boundaryStage !== selectedStage) {
    throw new Error(`IAM permissions boundary stage ${boundaryStage} does not match SST stage ${selectedStage}`)
  }
  return boundaryStage
}

const ACCOUNT_ID_PATTERN = /^\d{12}$/

/*
 * Mirrors the `${$app.name}-${$app.stage}-runtime-boundary` interpolation in stack/deploy.ts. The
 * bootstrap that provisions the policy, the CI preflight that checks the deploy role may attach it,
 * and the SST run that attaches it must agree on this name without importing each other's domain,
 * which is why it is spelled here rather than in any one of them.
 */
export function runtimeBoundaryPolicyArn({ accountId, appName, stage }: any) {
  if (!accountId || !ACCOUNT_ID_PATTERN.test(accountId)) {
    throw new Error(`accountId '${accountId}' must be a 12-digit AWS account id`)
  }
  // Both take the stage grammar: each is a segment of a generated AWS name, so each is bound by the
  // same character set and budget.
  if (!appName || !SST_STAGE_PATTERN.test(appName)) {
    throw new Error(`appName '${appName}' must match ${SST_STAGE_PATTERN}`)
  }
  validateSstStage(stage)
  return `arn:aws:iam::${accountId}:policy/${appName}-${stage}-runtime-boundary`
}

function validateOidcIssuer(name: any, value: any) {
  if (value !== value.trim()) {
    throw new Error(`${name} must not contain leading or trailing whitespace`)
  }

  let issuer
  try {
    issuer = new URL(value)
  } catch {
    throw new Error(`${name} must be a valid absolute HTTPS URL`)
  }

  if (issuer.protocol !== 'https:') {
    throw new Error(`${name} must use HTTPS`)
  }
  if (issuer.username || issuer.password) {
    throw new Error(`${name} must not include credentials`)
  }
  if (issuer.search || issuer.hash) {
    throw new Error(`${name} must not include a query string or fragment`)
  }

  return value
}

export function requireOidcIssuer(environment = process.env) {
  const value = environment.OIDC_ISSUER_BASE_URL
  if (!value) {
    throw new Error('OIDC_ISSUER_BASE_URL is required (e.g. https://<tenant>.auth0.com/)')
  }
  return validateOidcIssuer('OIDC_ISSUER_BASE_URL', value)
}

export function optionalPublicOidcIssuer(environment = process.env) {
  const value = environment.PUBLIC_OIDC_DOMAIN
  if (value === undefined || value === '') return undefined
  return validateOidcIssuer('PUBLIC_OIDC_DOMAIN', value)
}

export function resolveReleaseVersion(workspaceVersion: any, environment = process.env) {
  if (typeof workspaceVersion !== 'string' || workspaceVersion.trim() === '') {
    throw new Error('The workspace release version is missing')
  }
  if (workspaceVersion !== workspaceVersion.trim() || !STABLE_SEMVER_PATTERN.test(workspaceVersion)) {
    throw new Error(
      `The workspace release version '${workspaceVersion}' is not a stable semantic version (expected X.Y.Z)`,
    )
  }

  const configuredVersion = environment.VERSION
  if (configuredVersion === undefined || configuredVersion === '') return workspaceVersion
  if (configuredVersion !== configuredVersion.trim() || !STABLE_SEMVER_PATTERN.test(configuredVersion)) {
    throw new Error(`VERSION '${configuredVersion}' is not a stable semantic version (expected X.Y.Z)`)
  }
  return configuredVersion
}

function isFile(path: any) {
  try {
    return statSync(path).isFile()
  } catch {
    return false
  }
}

function findRepositoryRoot(moduleDirectory: any) {
  if (typeof moduleDirectory !== 'string' || moduleDirectory === '') {
    throw new Error('moduleDirectory must be a non-empty filesystem path')
  }

  const startingDirectory = resolve(moduleDirectory)
  let candidateDirectory = startingDirectory

  while (true) {
    const cargoTomlPath = join(candidateDirectory, 'Cargo.toml')
    const infraPackagePath = join(candidateDirectory, 'apps', 'infra', 'package.json')
    if (isFile(cargoTomlPath) && isFile(infraPackagePath)) return candidateDirectory

    const parentDirectory = dirname(candidateDirectory)
    if (parentDirectory === candidateDirectory) break
    candidateDirectory = parentDirectory
  }

  throw new Error(
    `could not find the BoxLite repository root from '${startingDirectory}' ` +
      '(expected one ancestor containing both Cargo.toml and apps/infra/package.json)',
  )
}

export function readWorkspaceVersion({ moduleDirectory = DEFAULT_MODULE_DIRECTORY } = {}) {
  const repositoryRoot = findRepositoryRoot(moduleDirectory)
  const cargoTomlPath = join(repositoryRoot, 'Cargo.toml')
  let cargoToml
  try {
    cargoToml = readFileSync(cargoTomlPath, 'utf8')
  } catch (cause) {
    throw new Error(`could not read workspace release version from '${cargoTomlPath}'`, { cause })
  }
  const version = cargoToml.match(/^version\s*=\s*"(.+?)"/m)?.[1]
  if (!version) {
    throw new Error(
      `could not parse release version from '${cargoTomlPath}' (expected a top-level \`version = "X.Y.Z"\`)`,
    )
  }
  return version
}

function requireHostname(name: any, value: any) {
  if (!value || value !== value.trim()) {
    throw new Error(`${name} must be a hostname without whitespace`)
  }

  let url
  try {
    url = new URL(`https://${value}`)
  } catch {
    throw new Error(`${name} must be a valid hostname`)
  }
  if (
    url.hostname !== value.toLowerCase() ||
    url.port ||
    url.username ||
    url.password ||
    url.pathname !== '/' ||
    url.search ||
    url.hash
  ) {
    throw new Error(`${name} must be a hostname without a scheme, port, path, credentials, query, or fragment`)
  }
  return url.hostname
}

/*
 * Reject a malformed entry here, where it costs nothing, rather than after SST
 * has begun mutating the stack.
 *
 * The parsed images are deliberately not returned. A post-deploy check compared
 * them to /api/config, which carries no image list yet: that field is the
 * acceptance criterion for #1045, and the check landed ahead of the API work it
 * tests, so every apply that set this variable failed (#1119). Reinstate the
 * comparison together with #1045, not before.
 */
function validateConfiguredSystemImages(rawImages: any) {
  for (const entry of (rawImages ?? '')
    .split(',')
    .map((entry: any) => entry.trim())
    .filter(Boolean)) {
    const separator = entry.indexOf('=')
    const name = separator > 0 ? entry.slice(0, separator).trim() : ''
    const ref = separator > 0 ? entry.slice(separator + 1).trim() : ''
    if (!name || !ref) {
      throw new Error(`Invalid BOXLITE_SYSTEM_IMAGES entry '${entry}', expected 'name=ref'`)
    }
  }
}

/*
 * The domain SES sends as, and therefore the identity the stack verifies.
 *
 * Shared with bootstrap rather than read twice: `--provision-ses` pins the SMTP
 * user's IAM policy to this identity's ARN, so a stage whose bootstrap and deploy
 * disagreed about the domain would mint a credential that authenticates and then
 * has every send refused.
 */
const DEFAULT_MAIL_DOMAIN = 'mail.boxlite.ai'

export function resolveMailDomain(environment = process.env) {
  return requireHostname('MAIL_DOMAIN', environment.MAIL_DOMAIN?.trim() || DEFAULT_MAIL_DOMAIN)
}

/*
 * The regional SES SMTP endpoint, which is both what the Api authenticates against
 * and what an operator types into Auth0's provider. One derivation for the deploy
 * and for bootstrap: the SMTP password is derived per region, so a host and a
 * credential from different regions authenticate nowhere.
 */
export function sesSmtpEndpoint(region: string) {
  if (!region) throw new Error('an SES SMTP endpoint needs a region')
  return `email-smtp.${region}.amazonaws.com`
}

export function resolvePublicDeploymentConfig(environment = process.env, workspaceVersion = readWorkspaceVersion()) {
  const stackDomain = requireHostname('STACK_DOMAIN', environment.STACK_DOMAIN)

  const proxyDomain = requireHostname('PROXY_DOMAIN', environment.PROXY_DOMAIN?.trim() || `proxy.${stackDomain}`)
  const proxyProtocol = environment.PROXY_PROTOCOL?.trim() || 'https'
  if (proxyProtocol !== 'https') {
    throw new Error(`PROXY_PROTOCOL must be https for the provisioned TLS NLB, received ${proxyProtocol}`)
  }
  const expectedProxyTemplateUrl = environment.PROXY_TEMPLATE_URL?.trim() || `${proxyProtocol}://${proxyDomain}`
  let proxyTemplateUrl
  try {
    proxyTemplateUrl = new URL(expectedProxyTemplateUrl)
  } catch {
    throw new Error('PROXY_TEMPLATE_URL must be a valid absolute HTTPS URL')
  }
  if (
    proxyTemplateUrl.protocol !== 'https:' ||
    proxyTemplateUrl.username ||
    proxyTemplateUrl.password ||
    proxyTemplateUrl.port ||
    proxyTemplateUrl.pathname !== '/' ||
    proxyTemplateUrl.search ||
    proxyTemplateUrl.hash
  ) {
    throw new Error('PROXY_TEMPLATE_URL must be an HTTPS origin without credentials, port, path, query, or fragment')
  }
  if (proxyTemplateUrl.hostname !== proxyDomain) {
    throw new Error(`PROXY_TEMPLATE_URL host ${proxyTemplateUrl.hostname} does not match PROXY_DOMAIN ${proxyDomain}`)
  }
  const expectedOidcIssuer = optionalPublicOidcIssuer(environment) ?? requireOidcIssuer(environment)
  const proxyTemplateOrigin = proxyTemplateUrl.origin
  const releaseVersion = resolveReleaseVersion(workspaceVersion, environment)
  validateConfiguredSystemImages(environment.BOXLITE_SYSTEM_IMAGES)

  return {
    stackDomain,
    proxyDomain,
    proxyProtocol,
    proxyTemplateUrl: proxyTemplateOrigin,
    releaseVersion,
    proxyHealthUrl: `${proxyProtocol}://${proxyDomain}/health`,
    proxyWildcardHealthUrl: `${proxyProtocol}://deployment-probe.${proxyDomain}/health`,
    apiConfigUrl: `https://api.${stackDomain}/api/config`,
    expectedOidcIssuer,
    expectedProxyTemplateUrl: proxyTemplateOrigin,
    expectedVersion: releaseVersion,
  }
}
