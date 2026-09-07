// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2024 Daytona Platforms Inc.
// Modified by BoxLite AI, 2025-2026
// Modified and rebranded for BoxLite

/// <reference path="../.sst/platform/config.d.ts" />

import { PRODUCTION_STAGE } from './settings.js'
import { createFoundation } from './foundation.js'
import { buildObservability } from './observability.js'
import { buildApi } from './api.js'
import { buildEdge } from './edge.js'
import { buildRunners } from './runners.js'
import { buildClickHouseStorage, buildClickHouseWriterReady } from './clickhouse.js'
import { buildMail } from './mail.js'

export async function deployStack() {
  const {
    optionalPublicOidcIssuer,
    readWorkspaceVersion,
    requireIamPermissionsBoundaryStage,
    requireOidcIssuer,
    resolveAwsRegion,
    resolveMailDomain,
    resolvePublicDeploymentConfig,
    sesSmtpEndpoint,
  } = await import('../deployment/environment.js')
  // eslint-disable-next-line @nx/enforce-module-boundaries -- Stack synthesis shares the policy host's CommonJS Runner model.
  const { resolveRunnerInventory } = await import('../runner/model/inventory.js')
  const REGION = resolveAwsRegion()
  const { accountId } = await aws.getCallerIdentity()
  const workspaceVersion = readWorkspaceVersion()
  const deploymentConfig = resolvePublicDeploymentConfig(process.env, workspaceVersion)
  const { stackDomain, proxyDomain, proxyProtocol, proxyTemplateUrl, releaseVersion } = deploymentConfig
  const runnerInventory = resolveRunnerInventory(process.env)
  const oidcIssuer = requireOidcIssuer()
  const publicOidcIssuer = optionalPublicOidcIssuer()

  // Every role created by this stack must stay inside the boundary provisioned
  // with the GitHub deployment role. The raw-resource transform also covers IAM
  // roles created internally by SST components, not only the roles declared here.
  requireIamPermissionsBoundaryStage($app.stage)
  const runtimePermissionsBoundaryArn = $interpolate`arn:aws:iam::${aws.getCallerIdentityOutput().accountId}:policy/${$app.name}-${$app.stage}-runtime-boundary`
  $transform(aws.iam.Role, (args: any) => {
    args.permissionsBoundary ??= runtimePermissionsBoundaryArn
  })

  // Strip trailing slash from service.url so path concat produces clean URLs
  // (api.url = "https://api.dev.boxlite.ai/" → apiBase = "https://api.dev.boxlite.ai").
  const stripTrailingSlash = (url: $util.Output<string>) => url.apply((u) => (u.endsWith('/') ? u.slice(0, -1) : u))

  // HTTPS everywhere: the Router CloudFront Function deletes customOriginConfig
  // for http origins and CF then falls back to match-viewer (→ tries HTTPS on a
  // port-80-only ALB → 502). We side-step that by giving Api and Dex ALBs
  // HTTPS listeners with a wildcard ACM cert, so Router routes to https://
  // origins and the non-buggy branch runs.
  const cloudflareDns = sst.cloudflare.dns()
  const serviceDomain = (name: string) => ({
    name: `${name}.${stackDomain}`,
    dns: cloudflareDns,
  })

  // ─── 1. SECRETS ──────────────────────────────────────────────────────────
  // Auto-generated — override any one by setting the matching env var.
  const randomKey = (name: string, length = 32) => new random.RandomPassword(name, { length, special: false })

  const encryptionKey = randomKey('EncryptionKey', 64)
  const encryptionSalt = randomKey('EncryptionSalt', 32)
  const proxyApiKey = randomKey('ProxyApiKey')
  const adminApiKey = randomKey('AdminApiKey')
  const defaultRunnerApiKey = randomKey('DefaultRunnerApiKey')
  const defaultRunnerConfig = runnerInventory[0]
  const defaultRunnerName = defaultRunnerConfig.controlPlaneRunnerName
  // App secrets — set via `npm run sst -- secret set <NAME> --stage <stage>`
  // (or bulk `npm run sst -- secret load <dotenv>`); stored encrypted in SST
  // state and shared per-stage by anyone with deploy access. OIDC_CLIENT_ID is
  // required and has no deployable fallback: a placeholder would let the stack
  // become healthy while every interactive login fails. Optional secrets carry
  // an empty-string fallback, where empty means that the feature is disabled.
  // NB: the Cloudflare provider creds can't live here (the provider initializes
  // in app() before run() exists); deployment/sst.ts injects them
  // from SSM instead.
  const oidcClientId = new sst.Secret('OIDC_CLIENT_ID')
  const oidcMgmtClientId = new sst.Secret('OIDC_MANAGEMENT_API_CLIENT_ID')
  const oidcMgmtClientSecret = new sst.Secret('OIDC_MANAGEMENT_API_CLIENT_SECRET')
  const posthogApiKey = new sst.Secret('POSTHOG_API_KEY', '')
  const svixAuthToken = new sst.Secret('SVIX_AUTH_TOKEN', '')
  // The API key the status sync presents to incident.io — scoped to alert
  // events + heartbeat pings only. Empty means the sync stays off; see
  // STATUS_SYNC_ENABLED below.
  const incidentIoToken = new sst.Secret('INCIDENT_IO_TOKEN', '')
  // The credential the usage exporter presents to Commerce's ingest route:
  // half of a shared secret whose other half is a Secrets Manager container
  // owned by boxlite-commerce's own stack, so both ends are set out of band
  // from one value rather than generated here.
  //
  // It is a secret of this stack rather than a read of that container
  // because the Api's *runtime* role could not read it if we tried. Its
  // execution role carries the boxlite-<stage>-runtime-boundary, which
  // admits only secret:boxlite-<stage>-* — deliberately, so one stage's
  // tasks cannot reach another's secrets. ECS says so plainly when asked:
  // it refuses to place the task with "no permissions boundary allows the
  // secretsmanager:GetSecretValue action". (The deploy role is not the
  // constraint — its boxlite-sst-deploy policy grants secretsmanager on
  // this stage's own secrets, and it carries no boundary at all.)
  //
  // Empty means the exporter stays off; see USAGE_EXPORT_ENABLED below.
  const usageExportToken = new sst.Secret('USAGE_EXPORT_TOKEN', '')

  // SES SMTP credentials for outbound mail (stack/mail.ts). Optional, so a
  // stage without them deploys and simply sends nothing.
  //
  // Secrets rather than stack resources because the deploy role cannot mint
  // them: bootstrap/aws/deploy-role-policy.json grants IAM on roles only, and
  // an SES SMTP credential is an IAM user's access key. `npm run bootstrap --
  // --provision-ses` creates that user with the operator's own credentials and
  // stores both halves here, the same shape as OIDC_CLIENT_ID above.
  const smtpUser = new sst.Secret('SMTP_USER', '')
  const smtpPassword = new sst.Secret('SMTP_PASSWORD', '')

  // ─── 2. PLATFORM ─────────────────────────────────────────────────────────
  // Network model + rationale (subnets / NAT / egress-only public IP, AWS citations): docs/networking.md
  // NAT instance (fck-nat, ~10× cheaper than a managed NAT Gateway). The Fargate
  // services run in private subnets (see Cluster below) with no public IP, so they
  // reach ECR, Docker Hub, the OIDC issuer, external ClickHouse, and AWS APIs
  // through this NAT. EC2 runners stay in public subnets and egress via the
  // Internet Gateway, not this NAT.
  // Durable state survives accidental teardown the way the runner does (§10).
  // `removal: 'retain'` (above) already keeps prod resources on `sst remove`, but it
  // does NOT stop a targeted destroy, a replace-on-immutable-change, or an AWS-console
  // delete — so prod also gets RDS deletion-protection + a final snapshot.
  // S3 versioning is on in every stage: cheap, and the only guard against an
  // object-level overwrite/delete (which `removal` never covers). Redis is a
  // transient cache, so it needs neither.
  const isProd = $app.stage === PRODUCTION_STAGE
  // Unique-but-stable suffix for the DB final snapshot: a fixed name would collide
  // with the snapshot a prior teardown of the same stage already created (RDS requires
  // unique final-snapshot ids). RandomId is stable across deploys (no drift) and is
  // regenerated on a full recreate, so each incarnation gets a distinct snapshot name.
  // Services run in PRIVATE subnets. SST's Vpc component otherwise defaults Fargate
  // tasks to public subnets with public IPs; passing the cluster a plain vpc object
  // (SST's documented escape hatch) overrides that: containerSubnets = private (no
  // public IP, egress via the NAT above), loadBalancerSubnets = public (ALBs stay
  // internet-facing, fronted by Cloudflare).
  const { vpc, db, redis, storage, cluster } = createFoundation(REGION, isProd)
  const foundation = { vpc, db, redis, storage, cluster }
  const clickHouseResources = await buildClickHouseStorage({
    foundation,
    region: REGION,
    accountId,
  })
  // One list for all three pipelines: traces used to fan out to Jaeger as well,
  // which was a second copy of spans ClickHouse already held.
  const collectorExporters = clickHouseResources.active ? '[boxlite_exporter,clickhouse]' : '[boxlite_exporter]'

  // ─── 3. IAM ──────────────────────────────────────────────────────────────
  // Box-storage credential vending. The Api's ECS task role assumes the
  // S3AccessRole declared after the Api service with a per-organization
  // inline session policy (apps/api object-storage.service.ts); effective
  // access is the intersection of the two. No IAM user / static keys: ECS
  // already delivers auto-rotated task-role credentials to the container.
  //
  // The role name is declared up front (deterministic, stage-scoped) so it
  // can go into the Api env and IAM grant as a plain string. The role
  // itself can only be created after the Api service, because its trust
  // policy names the task role — which exists once the Api does. Declaring
  // the name first breaks that resource cycle.
  const s3AccessRoleName = `${$app.name}-${$app.stage}-s3-access`
  const s3AccessRoleArn = $interpolate`arn:aws:iam::${aws.getCallerIdentityOutput().accountId}:role/${s3AccessRoleName}`

  // ─── 4. AUTH ─────────────────────────────────────────────────────────────
  // OIDC is delegated to an external provider (Auth0/Okta/etc.) via
  // OIDC_ISSUER_BASE_URL. No in-cluster Dex — removes one ALB + ACM cert +
  // service and the ephemeral-sqlite key-rotation problem.
  //
  // Router still exists for dashboard HTTPS + routing /* to Api.
  // NOTE: SST Router's placeholder origin is created with
  // `OriginProtocolPolicy: "http-only"`, which wins over the per-request
  // customOriginConfig set by its CloudFront Function for HTTPS origins
  // (CF rejects the TLS handshake → 502). Flip it to `https-only` so CF
  // respects the CF-Function's HTTPS override.
  const router = new sst.aws.Router('ApiCdn', {
    domain: { name: stackDomain, dns: cloudflareDns },
    transform: {
      cdn: (cdnArgs: any) => {
        cdnArgs.origins = $util.output(cdnArgs.origins).apply((origins) =>
          (origins ?? []).map((o: any) => ({
            ...o,
            customOriginConfig: o.customOriginConfig
              ? { ...o.customOriginConfig, originProtocolPolicy: 'https-only', originReadTimeout: 60 }
              : o.customOriginConfig,
          })),
        )
      },
    },
  })

  // ─── 5. OBSERVABILITY INGEST ─────────────────────────────────────────────
  // Created before Api so API, runner, host, and box can all emit OTLP to the
  // same Collector. ClickHouse is either the private single-node backend or
  // an existing managed service; ECS stdout/stderr remains in CloudWatch.
  const { otelCollector, otelCollectorOtlpHttpUrl } = buildObservability({
    cluster,
    stackDomain,
    adminApiKey,
    clickHouseResources,
    collectorExporters,
    stripTrailingSlash,
  })
  const clickHouseWriterReadyDependency = buildClickHouseWriterReady({
    region: REGION,
    resources: clickHouseResources,
    otelCollector,
    otelCollectorOtlpHttpUrl,
  })

  // ─── 5b. OUTBOUND MAIL ───────────────────────────────────────────────────
  // Ahead of the Api because it produces that service's SMTP_* settings.
  // Verification blocks on DNS, so a first deploy of a new MAIL_DOMAIN waits
  // here rather than at the first send.
  const { smtpEnvironment } = buildMail({
    dns: cloudflareDns,
    senderDomain: resolveMailDomain(),
    smtpHost: sesSmtpEndpoint(REGION),
    smtpUser,
    smtpPassword,
  })

  // ─── 6. API (NestJS control plane) ───────────────────────────────────────
  // Where the Api image comes from. `release` deploys the image published for a version, so a
  // release promotes the exact artifact that was tested rather than rebuilding one that merely
  // shares its commit. `build` deploys the image its own CI job built for the selected commit —
  // the Runner has always worked that way, and doing it here too means a build deploy installs
  // bytes that were built once and can be pointed at again, rather than bytes this particular
  // deploy happened to compile.
  //
  // A build with no Api ref means nothing published an Api image for this checkout, so SST
  // builds apps/api/Dockerfile the way it always did. That is a plain local `npm run deploy`,
  // and also `npm run runner:build-artifact`, which stages a Runner and sets only the Runner's
  // ref. deploy-infra.yml publishes both and sets the global one.
  //
  // SST hands an image string straight to the task definition (normalizeImage, sst/platform
  // fargate component), so the modes differ only in this expression.
  //
  // The stage bootstrap (bootstrap/aws.ts's ensureApiImageRepository) owns the immutable
  // repository: an image has to be published before a fresh stack can consume one, so the
  // consumer cannot also be responsible for creating its input.
  const { api } = buildApi({
    foundation,
    region: REGION,
    accountId,
    releaseVersion,
    stackDomain,
    proxyDomain,
    proxyProtocol,
    proxyTemplateUrl,
    serviceDomain,
    s3AccessRoleName,
    s3AccessRoleArn,
    encryptionKey,
    encryptionSalt,
    proxyApiKey,
    adminApiKey,
    defaultRunnerApiKey,
    defaultRunnerName,
    oidcClientId,
    oidcMgmtClientId,
    oidcMgmtClientSecret,
    posthogApiKey,
    svixAuthToken,
    usageExportToken,
    incidentIoToken,
    oidcIssuer,
    publicOidcIssuer,
    otelCollectorOtlpHttpUrl,
    smtpEnvironment,
    clickHouseResources,
    clickHouseReadyDependency: clickHouseWriterReadyDependency,
  })

  // ─── 7. EDGE SERVICES ────────────────────────────────────────────────────
  buildEdge({
    foundation,
    api,
    router,
    proxyDomain,
    proxyProtocol,
    cloudflareDns,
    proxyApiKey,
    oidcClientId,
    oidcIssuer,
    publicOidcIssuer,
    otelCollectorOtlpHttpUrl,
    stripTrailingSlash,
  })

  // ─── 10. RUNNER (EC2 with nested KVM) ────────────────────────────────────
  // Boots an Ubuntu EC2 that runs the prebuilt runner binary under systemd, with nested KVM
  // enabled for box VMs.
  //
  // Where that binary comes from is the mirror of the Api's choice above. `release` — the
  // default, and all this stack could do before — installs the published GitHub Release asset
  // for a version. `build` installs a binary produced from the deployed commit and staged in
  // the bucket below, which is what makes an unreleased Runner change testable at all.
  //
  // Both install paths (user-data at first boot, SSM for a live host) take the same
  // URL + checksum pair, so the source only changes where the two URLs point and which
  // command fetches them — see artifacts/runner.ts.
  //
  // The stage bootstrap template owns this private bucket for the same ordering reason
  // it owns the API repository: CI stages the object before this stack can consume it. The name
  // is derived in one helper shared with the preflight and the staging command.
  await buildRunners({
    foundation,
    api,
    otelCollectorOtlpHttpUrl,
    region: REGION,
    accountId,
    runnerInventory,
    defaultRunnerConfig,
    defaultRunnerApiKey,
    adminApiKey,
    randomKey,
  })
}
