/**
 * How long the credentials a run holds have to stay valid.
 *
 * One rule, in one place, because both engines apply it. It lives here rather
 * than in either of them: `deploy.ts` imports both engines, so a constant kept
 * there and read back by them would be a cycle, and a copy in each is the same
 * policy spelled twice.
 */

/** SST holds a per-stage lock and a deploy runs long; fail now rather than mid-rollout. */
export const REQUIRED_CREDENTIAL_SECONDS = 20 * 60

/**
 * A preview holds no lock and changes nothing, so it does not need the window a
 * rollout does — refusing to *look* because a session expires in ten minutes
 * would be refusing the cheapest thing available.
 */
export const REQUIRED_PREVIEW_SECONDS = 60

/**
 * A teardown runs as long as a rollout and holds the same lock, so it needs the
 * same window; only the preview is cheap enough to do without one.
 */
export const windowFor = (intent: 'deploy' | 'diff' | 'remove'): number =>
  intent === 'diff' ? REQUIRED_PREVIEW_SECONDS : REQUIRED_CREDENTIAL_SECONDS
