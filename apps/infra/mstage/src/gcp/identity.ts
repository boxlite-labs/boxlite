/*
 * Who a command is running as on GCP.
 *
 * Three differences from AWS, all forced by the platform rather than chosen:
 *
 * Nothing expires on a clock this can read. Application Default Credentials
 * refresh themselves, so `expiresAt` answers null and `assertUsableFor` has
 * nothing to check. That is the honest answer — inventing a deadline would be a
 * guarantee this cannot keep, and reporting one that never arrives would be a
 * guard that never fires.
 *
 * A child inherits a path, not a key. AWS hands three variables that are the
 * credential; Google hands a file the SDK reads and re-reads, so the child gets
 * `GOOGLE_APPLICATION_CREDENTIALS` and the project, and every AWS variable is
 * cleared so a subprocess cannot pick up the other cloud by accident.
 *
 * The tenant is a project, and the project is a string the caller already knows.
 * There is no `GetCallerIdentity` equivalent worth a network call here, so
 * `whoami` asks the auth library for the project it is pointed at.
 */

import type { Caller, Identity } from '../identity.ts'
import type { Scope } from '../aws/precedence.ts'

/**
 * What Google's auth library answers, in the shape it already has. Structural
 * rather than imported so nothing depends on the package being installed until
 * a GCP stage exists.
 */
export type GoogleAuth = {
  getProjectId: () => Promise<string>
  getCredentials: () => Promise<{ client_email?: string }>
}

export type GcpIdentity = Identity & { readonly home: 'gcp' }

/** Where the credentials file is, when one is being used rather than a metadata server. */
const CREDENTIALS_VARIABLE = 'GOOGLE_APPLICATION_CREDENTIALS'

export const resolveGcpIdentity = ({
  scope,
  auth,
  environment = process.env,
}: {
  scope: Scope
  auth: GoogleAuth
  environment?: NodeJS.ProcessEnv
}): GcpIdentity => {
  let caller: Caller | null = null

  const whoami = async (): Promise<Caller> => {
    if (caller) return caller
    const [project, credentials] = await Promise.all([auth.getProjectId(), auth.getCredentials()])
    caller = { tenant: project, ...(credentials.client_email ? { principal: credentials.client_email } : {}) }
    return caller
  }

  return {
    home: 'gcp',
    region: scope.region,
    stage: scope.stage,
    app: scope.app,
    whoami,

    // Application Default Credentials refresh themselves. There is no deadline
    // to report, and reporting one would be a promise this cannot keep.
    async expiresAt() {
      return null
    },
    async assertUsableFor() {
      return
    },

    /**
     * A child gets the project and, when one is in use, the path to the
     * credentials. Every AWS variable is cleared: a subprocess that found a
     * stale key triple would authenticate to the other cloud and fail somewhere
     * that mentions neither.
     */
    async childEnvironment(base = environment) {
      const env: NodeJS.ProcessEnv = { ...base }
      delete env.AWS_PROFILE
      delete env.AWS_ACCESS_KEY_ID
      delete env.AWS_SECRET_ACCESS_KEY
      delete env.AWS_SESSION_TOKEN
      delete env.AWS_REGION
      delete env.AWS_DEFAULT_REGION

      const { tenant } = await whoami()
      if (tenant) {
        env.GOOGLE_CLOUD_PROJECT = tenant
        // gcloud and the Pulumi provider read this one rather than the above.
        env.CLOUDSDK_CORE_PROJECT = tenant
      }
      env.CLOUDSDK_COMPUTE_REGION = scope.region
      const credentials = base?.[CREDENTIALS_VARIABLE]
      if (credentials) env[CREDENTIALS_VARIABLE] = credentials

      return { env, expiresAt: null }
    },
  }
}
