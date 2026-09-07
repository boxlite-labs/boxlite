/*
 * Who a command is running as, on whichever cloud the stage lives in.
 *
 * Three questions, and none of them says access key, service account or role.
 * They are the three every caller in this repository actually asks:
 *
 *   whoami            who this is, to print or to name in an ARN
 *   assertUsableFor   will this still be valid when a long thing finishes
 *   childEnvironment  what a subprocess needs to inherit to be this identity
 *
 * The last one is where the clouds differ most and why this interface exists at
 * all. AWS hands a child three variables and they do not refresh, so a deploy
 * has to check the clock before it starts. Google hands a child a path to
 * credentials that refresh themselves, so the same check has almost nothing to
 * say — and pretending otherwise would mean either a false guarantee on one
 * cloud or a missing one on the other.
 *
 * `tenant` is the word for the thing an identity belongs to: an account id on
 * AWS, a project on GCP. Naming it neither keeps `whoami` readable on both.
 */

/** What a caller can say about the identity it is running as. */
export type Caller = {
  /** Account id, or project. Whichever this cloud calls the thing it belongs to. */
  tenant?: string
  /** Something a person can recognise: an assumed role's ARN, an email. */
  principal?: string
}

export type Identity = {
  /** Which cloud this identity belongs to. */
  readonly home: string
  region: string
  stage: string | null
  app: string | null
  whoami: () => Promise<Caller>
  /**
   * When these credentials stop working, or null when they refresh themselves.
   * Null is an answer, not a gap: it means the clock is not a risk here.
   */
  expiresAt: () => Promise<Date | null>
  /** Refuses to start long work that would outlive the credentials. */
  assertUsableFor: (seconds: number, now?: () => Date) => Promise<void>
  /**
   * The environment a subprocess inherits so it is this identity and nothing
   * else. Built from `base` with every competing variable removed, because a
   * leftover profile or a stale key is how a deploy silently runs as someone
   * else.
   */
  childEnvironment: (base?: NodeJS.ProcessEnv) => Promise<{ env: NodeJS.ProcessEnv; expiresAt: Date | null }>
}
