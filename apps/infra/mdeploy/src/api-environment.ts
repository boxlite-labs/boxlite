/*
 * What the API container reads.
 *
 * Assembled here rather than in `sst.config.ts` because none of it needs SST:
 * it is the stage's own configuration rearranged into the names the service
 * expects. A function handed its environment is one a test can run; one reading
 * `process.env` through an SST config is not, which is how a key stops being
 * passed without anything noticing. The incumbent `stack/api.ts` was the second
 * kind, which is why its two hundred lines of `envOr` had no test at all.
 *
 * Two channels come out of it, because a container reads a name the same way
 * whichever one carried it. `environment` is delivered as values; `secrets` is
 * delivered as addresses that the platform resolves before the container
 * starts, so those values never enter a task definition or a revision. Which
 * keys go which way is `mstage.config.json`'s answer, not this file's:
 * `env.selectGroup.api` names what the API reads, and `env.selectGroup.secret`
 * marks which of those are addresses.
 *
 * The database's, the cache's and the collector's own names are added by the
 * stack on top of this, because those modules own them.
 */

import { API_GROUP, serviceSecretsFrom, splitServiceChannels, type Environment, type GroupDeclaration } from './env.ts'
import { SECRET_GROUP } from 'mstage/secret-address'
import type { MstageConfig } from 'mstage/config'

export class ApiEnvironmentError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'ApiEnvironmentError'
  }
}

const optional = (environment: Environment, key: string): string | null => environment[key]?.trim() || null

/** A stage flag, rejected here rather than at the service's boot. */
const flag = (environment: Environment, key: string): boolean => {
  const value = optional(environment, key) ?? 'false'
  if (value !== 'true' && value !== 'false') throw new ApiEnvironmentError(`${key} must be true or false`)
  return value === 'true'
}

/**
 * A feature switched on by holding a credential, not by a second setting.
 *
 * Three of the API's features work this way — usage export, the status sync,
 * and the Auth0 management client — and each refuses to boot when its flag is
 * on and its credential is missing. Deriving the flag from the credential is
 * what stops a stage that was pointed at a service but never given the secret
 * from crash-looping instead of simply staying dark.
 *
 * Derived from *presence in the store* rather than from the value's text, and
 * that distinction is load-bearing: a credential delivered by reference is an
 * address here, so reading the text would ask whether an ARN is empty. Either
 * channel counts as held, which is the only answer that stays true when a
 * secret moves from one to the other.
 */
const held = (delivered: Record<string, string>, key: string): boolean => (delivered[key]?.trim() ?? '') !== ''

/**
 * The names the billing read path only means anything as a set.
 *
 * Exported so a test can hold it against `env.selectGroup`: a name this
 * function expects but no group fetches would otherwise read as "billing not
 * configured" on every deploy, with nothing saying why.
 */
export const BILLING_KEYS = ['BILLING_API_URL', 'USAGE_EXPORT_URL', 'USAGE_EXPORT_TOKEN']

/** The same, for the incident.io status sync. */
export const STATUS_SYNC_KEYS = [
  'INCIDENT_IO_ALERT_SOURCE_CONFIG_ID',
  'INCIDENT_IO_HEARTBEAT_ID',
  'INCIDENT_IO_TOKEN',
  'STATUS_SYNC_DEDUP_PREFIX',
]

/**
 * The dashboard's two origins.
 *
 * They are different on purpose. Static assets are served through the CDN at
 * the root domain; the dashboard's API client talks to `api.<domain>` directly,
 * because CloudFront caps a WebSocket at ten minutes and times an origin read
 * out at sixty seconds — which breaks `/attach`, build-log streaming and file
 * uploads. Both are overridable, because a stage may put either behind
 * something else, and both have a default so a stage that overrides neither is
 * still correct.
 */
const dashboardFrom = (environment: Environment, domain: string): Record<string, string> => ({
  DASHBOARD_URL: optional(environment, 'DASHBOARD_URL') ?? `https://${domain}`,
  DASHBOARD_BASE_API_URL: optional(environment, 'DASHBOARD_BASE_API_URL') ?? `https://api.${domain}`,
  ...(optional(environment, 'APP_URL') ? { APP_URL: optional(environment, 'APP_URL') as string } : {}),
})

/**
 * The OIDC settings, and the management client the deploy may not be able to
 * configure.
 *
 * `OIDC_MANAGEMENT_API_ENABLED` is the one flag that is *not* derived from its
 * credential, and the asymmetry is deliberate: the audience it needs is a
 * setting rather than a secret, and a stage that turned the feature on without
 * one would have the API fail at its first account-link rather than at boot.
 * Refusing here names the missing key.
 */
const oidcFrom = (environment: Environment, domain: string): Record<string, string> => {
  const issuer = optional(environment, 'OIDC_ISSUER_BASE_URL')
  if (!issuer) {
    throw new ApiEnvironmentError(
      'OIDC_ISSUER_BASE_URL is required — every interactive login goes through it, and a placeholder ' +
        'would let the stack become healthy while nobody can sign in',
    )
  }
  const management = flag(environment, 'OIDC_MANAGEMENT_API_ENABLED')
  const audience = optional(environment, 'OIDC_MANAGEMENT_API_AUDIENCE')
  if (management && !audience) {
    throw new ApiEnvironmentError('OIDC_MANAGEMENT_API_AUDIENCE is required when OIDC_MANAGEMENT_API_ENABLED=true')
  }
  const passthrough = (key: string) => (optional(environment, key) ? { [key]: optional(environment, key) as string } : {})
  return {
    OIDC_ISSUER_BASE_URL: issuer,
    OIDC_AUDIENCE: optional(environment, 'OIDC_AUDIENCE') ?? 'boxlite',
    // Safe to set unconditionally: the API probes the issuer's discovery
    // document at boot and only offers this fallback to the dashboard when the
    // issuer itself advertises no end_session_endpoint.
    OIDC_END_SESSION_ENDPOINT:
      optional(environment, 'OIDC_END_SESSION_ENDPOINT') ?? `https://${domain}/api/auth/end-session`,
    ...passthrough('PUBLIC_OIDC_DOMAIN'),
    ...passthrough('OIDC_POST_LOGOUT_REDIRECT_ALLOWLIST'),
    ...(management
      ? {
          OIDC_MANAGEMENT_API_ENABLED: 'true',
          OIDC_MANAGEMENT_API_AUDIENCE: audience as string,
          ...passthrough('OIDC_MANAGEMENT_API_BASE_URL'),
          ...passthrough('OIDC_MANAGEMENT_API_TOKEN_URL'),
        }
      : {}),
  }
}

/**
 * Where the dashboard's billing surface calls, and where finalized usage goes.
 *
 * `USAGE_EXPORT_URL` is derived from `BILLING_API_URL`'s origin rather than
 * taken as a second setting. The publisher appends `/internal/usage-events`,
 * which the billing service answers off its bare origin because that route
 * authenticates a service rather than a user — so sending the billing base path
 * would 404 every batch, and a second setting is a second thing that can be
 * pointed somewhere else.
 *
 * No default for `BILLING_API_URL`: this stack deploys no billing service, so
 * a stage that names none keeps the dashboard's billing pages gated off, which
 * is the correct state rather than a broken one.
 */
const billingFrom = (environment: Environment, delivered: Record<string, string>): Record<string, string> => {
  const base = optional(environment, 'BILLING_API_URL')
  if (!base) return {}
  let origin: string
  try {
    origin = new URL(base).origin
  } catch {
    throw new ApiEnvironmentError(`BILLING_API_URL must be an absolute URL; got ${JSON.stringify(base)}`)
  }
  const exporting = held(delivered, 'USAGE_EXPORT_TOKEN')
  return {
    BILLING_API_URL: base,
    USAGE_EXPORT_URL: optional(environment, 'USAGE_EXPORT_URL') ?? origin,
    USAGE_EXPORT_ENABLED: String(exporting),
    // The same destination and credential carry the periodic snapshot the
    // billing service estimates still-open usage from.
    USAGE_ALLOCATION_SNAPSHOT_ENABLED: String(exporting),
  }
}

/** The status sync, gated on the alert source id so an unconfigured stage carries none of it. */
const statusSyncFrom = (
  environment: Environment,
  delivered: Record<string, string>,
  stage: string,
): Record<string, string> => {
  const source = optional(environment, 'INCIDENT_IO_ALERT_SOURCE_CONFIG_ID')
  if (!source) return {}
  return {
    INCIDENT_IO_ALERT_SOURCE_CONFIG_ID: source,
    ...(optional(environment, 'INCIDENT_IO_HEARTBEAT_ID')
      ? { INCIDENT_IO_HEARTBEAT_ID: optional(environment, 'INCIDENT_IO_HEARTBEAT_ID') as string }
      : {}),
    STATUS_SYNC_ENABLED: String(held(delivered, 'INCIDENT_IO_TOKEN')),
    // Every deployed stage reports ENVIRONMENT=production, so the per-stage
    // alert identity comes from the stage name instead.
    STATUS_SYNC_DEDUP_PREFIX: optional(environment, 'STATUS_SYNC_DEDUP_PREFIX') ?? `boxlite-${stage}`,
  }
}

/**
 * The registry boxes pull their system images from.
 *
 * All of nothing: a URL with no credential is a registry every box pull fails
 * against, and a credential with no URL configures nothing. Gated on the URL,
 * which is the value that says a stage meant to use one at all.
 */
const systemImagesFrom = (environment: Environment): Record<string, string> => {
  const passthrough = (key: string, fallback = '') => ({ [key]: optional(environment, key) ?? fallback })
  const registry = optional(environment, 'BOXLITE_SYSTEM_SOURCE_REGISTRY_URL')
  return {
    ...passthrough('BOXLITE_SYSTEM_BASE_IMAGE'),
    ...passthrough('BOXLITE_SYSTEM_NODE_IMAGE'),
    ...passthrough('BOXLITE_SYSTEM_PYTHON_IMAGE'),
    ...passthrough('BOXLITE_SYSTEM_IMAGE_TAG'),
    ...passthrough('BOXLITE_SYSTEM_IMAGES'),
    ...(registry
      ? {
          BOXLITE_SYSTEM_SOURCE_REGISTRY_URL: registry,
          ...passthrough('BOXLITE_SYSTEM_SOURCE_REGISTRY_NAME', 'BoxLite System Source Registry'),
          ...passthrough('BOXLITE_SYSTEM_SOURCE_REGISTRY_USERNAME'),
          ...passthrough('BOXLITE_SYSTEM_SOURCE_REGISTRY_PROJECT_ID'),
        }
      : {}),
  }
}

/**
 * The two channels the API reads, assembled from one stage's configuration.
 *
 * `region` is here because the object-storage endpoints are regional and the
 * API composes S3 and STS URLs from them; `stage` because the status sync's
 * alert identity is per stage. Neither is read from an ambient variable —
 * mstage already resolved both, and asking again is a second answer.
 */
export const apiEnvironmentFrom = ({
  environment,
  declaration,
  region,
  stage,
  home,
}: {
  environment: Environment
  declaration: GroupDeclaration
  region: string
  stage: string
  home: MstageConfig['home']
}): { environment: Record<string, string>; secrets: Record<string, string> } => {
  const domain = optional(environment, 'STACK_DOMAIN')
  if (!domain) throw new ApiEnvironmentError('STACK_DOMAIN is required — every URL the API composes starts with it')

  // The API's own group, before it is split by channel. Read once: `held`
  // below asks whether a credential arrived, and asking the raw environment
  // would miss one delivered as an address.
  const delivered = serviceSecretsFrom({ group: API_GROUP, declaration, environment })
  const { values, addresses } = splitServiceChannels({ delivered, declaration, home })

  return {
    environment: {
      // Constant on every deployed stage. The per-stage identity is the stage
      // name, which is what the status sync's dedup prefix uses.
      ENVIRONMENT: 'production',
      PORT: String(3000),
      S3_REGION: region,
      OTEL_ENABLED: String(!flag(environment, 'OTEL_DISABLED')),
      ...dashboardFrom(environment, domain),
      ...oidcFrom(environment, domain),
      ...billingFrom(environment, delivered),
      ...statusSyncFrom(environment, delivered, stage),
      ...systemImagesFrom(environment),
      ...(optional(environment, 'POSTHOG_HOST') ? { POSTHOG_HOST: optional(environment, 'POSTHOG_HOST') as string } : {}),
      ...(optional(environment, 'SVIX_SERVER_URL')
        ? { SVIX_SERVER_URL: optional(environment, 'SVIX_SERVER_URL') as string }
        : {}),
      ...(optional(environment, 'BOX_MIGRATION_ARCHIVE_PREFIX')
        ? { BOX_MIGRATION_ARCHIVE_PREFIX: optional(environment, 'BOX_MIGRATION_ARCHIVE_PREFIX') as string }
        : {}),
      ...(optional(environment, 'DEFAULT_REGION_ID')
        ? { DEFAULT_REGION_ID: optional(environment, 'DEFAULT_REGION_ID') as string }
        : {}),
      ...(optional(environment, 'DEFAULT_TEMPLATE')
        ? { DEFAULT_TEMPLATE: optional(environment, 'DEFAULT_TEMPLATE') as string }
        : {}),
      // The first runner's registered name, which the API seeds at boot. The
      // fleet itself is the runner module's; this is the one name the API has
      // to agree with it about.
      DEFAULT_RUNNER_NAME: optional(environment, 'DEFAULT_RUNNER_NAME') ?? 'default',
      // The API's own group, as values. Last, so a name the store holds wins
      // over a default composed above — and `stack/index.ts` refuses any name
      // that a module also decides.
      ...values,
    },
    secrets: addresses,
  }
}

/**
 * Whether a key reaches the API by reference on this stage.
 *
 * Exported for the composition root's benefit rather than used here: it is the
 * one question a caller asks that this module can answer without re-deriving
 * the split.
 */
export const isAddress = (declaration: GroupDeclaration, key: string): boolean =>
  (declaration.groups[SECRET_GROUP] ?? []).includes(key)
