/*
 * Which cloud a stage lives in, resolved once.
 *
 * `mstage.config.json` declares `home` — for the repository, and per stage
 * where one differs — and `homeFor` composes those two into the single answer
 * that reaches this file on the scope. This is the only place that answer turns
 * into an identity, a store backend and the bucket both of them sit in.
 * Everything above it — every `env` command, every caller that asks who it is
 * running as — works against those interfaces and never learns which cloud
 * answered.
 *
 * The mirror of mdeploy's per-cloud provider bundles: one file per repository
 * where the cloud is chosen, rather than a branch at every call site. What the
 * stage contributes is the coordinates that cloud needs — its region, and the
 * account or project it is pinned to.
 *
 * Google's SDKs are imported lazily and only when a GCP stage is what was
 * asked for. A repository whose stages all live in AWS therefore never has to
 * have them installed — and in a repository like this one, where only some
 * stages are on GCP, an AWS deploy never pays for them either.
 */

import { awsBackend, clientsFor, readStateBucket as awsStateBucket } from './env/aws-backend.ts'
import { gcpBackend, readStateBucket as gcpStateBucket, type GcpClients } from './env/gcp-backend.ts'
import { resolveIdentity, type AwsIdentity } from './aws/identity.ts'
import { resolveGcpIdentity, type GcpIdentity, type GoogleAuth } from './gcp/identity.ts'
import type { StoreBackend } from './env/backend.ts'
import type { Scope } from './aws/precedence.ts'

export class HomeError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'HomeError'
  }
}

/** What every cloud answers with, once the stage's declaration has chosen one. */
type Access = {
  backend: StoreBackend
  /**
   * The bucket this stage's state sits in — the store on both clouds, and on
   * GCP the Pulumi backend as well, because a stage that keeps its state in the
   * project it deploys into keeps both in one place.
   *
   * A function rather than a value: reading it costs a lookup against the
   * bootstrap record, and most commands never ask.
   */
  stateBucket: () => Promise<string>
}

/**
 * One resolved cloud: who you are, where the configuration is, where the state is.
 *
 * A union rather than one shape holding an `Identity`, so a caller can narrow on
 * `identity.home` and have the half that cloud actually offers. SST needs a
 * resolved AWS key triple and nothing else can produce one; a caller that had to
 * take the narrow interface and check at runtime for methods that are always
 * there would be re-deciding, by hand, the question this file already answered.
 */
export type Home = (Access & { identity: AwsIdentity }) | (Access & { identity: GcpIdentity })

/**
 * The Google clients, loaded only when a GCP stage asks for them.
 *
 * Injectable so a test can exercise the dispatch without the packages, which is
 * the same reason the backend describes their shape structurally instead of
 * importing their types.
 */
export type GoogleFactory = (input: { project: string }) => Promise<{ clients: GcpClients; auth: GoogleAuth }>

/*
 * The specifiers are constants rather than literals on purpose.
 *
 * mstage is shared by repositories that have never seen GCP, and a literal
 * would make `tsc -p mstage/tsconfig.build.json` require all three packages to
 * be installed in every one of them. Resolving them at runtime, in the only
 * repository that has adopted GCP, does not.
 */
const STORAGE = '@google-cloud/storage'
const SECRET_MANAGER = '@google-cloud/secret-manager'
const AUTH = 'google-auth-library'

/** Everything a Google client may do on this project's behalf. */
const CLOUD_PLATFORM = 'https://www.googleapis.com/auth/cloud-platform'

const loadGoogle: GoogleFactory = async ({ project }) => {
  let storage: any
  let secrets: any
  let auth: any
  try {
    const [storageModule, secretsModule, authModule] = await Promise.all([
      import(STORAGE),
      import(SECRET_MANAGER),
      import(AUTH),
    ])
    storage = new storageModule.Storage({ projectId: project })
    secrets = new secretsModule.SecretManagerServiceClient({ projectId: project })
    // Application Default Credentials: whatever this machine or runner already
    // proves. mstage does not choose credentials on either cloud — it verifies
    // the tenant the ones in hand resolve to.
    auth = new authModule.GoogleAuth({ projectId: project, scopes: [CLOUD_PLATFORM] })
  } catch (error) {
    throw new HomeError(
      'A GCP stage needs @google-cloud/storage, @google-cloud/secret-manager and google-auth-library. ' +
        `Install them in this repository, or pass a factory to resolveHome. (${(error as Error).message})`,
    )
  }
  /*
   * Cast at the seam, deliberately. `GcpClients` is mstage's own description of
   * the slice of these SDKs it uses (`env/gcp-backend.ts:29-49`), written
   * structurally so nothing above depends on the packages' types. This function
   * is the one place the real objects meet that description, so it is the one
   * place the assertion belongs.
   */
  return { clients: { storage, secrets } as GcpClients, auth: auth as GoogleAuth }
}

/**
 * The project a GCP stage lives in.
 *
 * Declared rather than discovered, unlike the AWS account: the Storage and
 * Secret Manager clients below cannot be built without a project, so there is
 * nothing to ask before it is known.
 */
const projectOf = (scope: Scope): string => {
  if (!scope.project) {
    throw new HomeError(`Stage "${scope.stage}" has no project; a GCP stage declares which project it lives in`)
  }
  return scope.project
}

export const resolveHome = async ({ scope, google = loadGoogle }: { scope: Scope; google?: GoogleFactory }): Promise<Home> => {
  // The scope's, not the config's: `resolveScope` has already folded the stage's
  // own declaration over the repository's default, and asking the file a second
  // time here is what would let one stage's home differ from the other's answer.
  switch (scope.home) {
    case 'aws': {
      const identity = resolveIdentity({ scope })
      const clients = clientsFor(identity)
      return { identity, backend: awsBackend(clients), stateBucket: () => awsStateBucket(clients) }
    }
    case 'gcp': {
      const project = projectOf(scope)
      const { clients, auth } = await google({ project })
      return {
        identity: resolveGcpIdentity({ scope, auth }),
        backend: gcpBackend({ clients, project }),
        stateBucket: () => gcpStateBucket(clients, project),
      }
    }
    default:
      throw new HomeError(`Unknown home "${scope.home}"; mstage keeps a stage's configuration in aws or gcp`)
  }
}
