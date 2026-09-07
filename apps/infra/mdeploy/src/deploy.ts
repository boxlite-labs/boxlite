/**
 * How BoxLite deploys, on whichever cloud the stage lives in.
 *
 * This is the half mstage deliberately does not contain. mstage creates and
 * verifies access — sign-ins, stage configuration, the identity — and every
 * repository shares it. Spending that access differs per repository: machine
 * shapes, images, rollout gates.
 *
 * The cloud is chosen once, here, and the answer is a bundle rather than a
 * boolean. `DeployTarget` is the mirror of `stack/index.ts`'s `StackProviders`
 * one layer up: the stack is written against providers and names no cloud, and
 * everything above this is written against a target and names none either.
 *
 * That shape is load-bearing rather than tidy. More than the engine differs —
 * the store the configuration comes from, which groups a run reads out of it,
 * the identity that opens both, and where the state lives are all per cloud. As
 * separate `=== 'gcp'` checks they were four call sites that had to agree, and
 * the fifth one added later would not have.
 */

import { resolveHome } from 'mstage/home'
import type { Cloud, MstageConfig } from 'mstage/config'
import type { StoreBackend } from 'mstage/env'
import type { Scope } from 'mstage/scope'
import { awsTarget } from './sst.ts'
import { gcpTarget } from './pulumi.ts'

/**
 * What the engine is asked to do. A preview reads; the other two write.
 *
 * `remove` is the same engine in the other direction, so it belongs here rather
 * than in a tool of its own: it holds the same per-stage lock, needs the same
 * identity and the same stage configuration, and every guard a deploy passes is
 * one a teardown has more reason to.
 */
export type Intent = 'deploy' | 'diff' | 'remove'

/** Which engine drives a run. One per cloud, and not a preference. */
export type Engine = 'sst' | 'pulumi'

/** One run, in the words every engine understands. */
export type DeployRequest = {
  /**
   * Deploy, preview, or tear down. The same environment, the same targets and
   * the same config in every case: a preview that resolved its inputs
   * differently would be previewing something else.
   */
  intent?: Intent
  /**
   * Component names to act on. Empty covers the whole stack, which is what a
   * local run wants; a workflow names one module's components so a failure says
   * which module failed. An engine that cannot select on them refuses rather
   * than translating.
   */
  targets?: string[]
  /** The stage's configuration, handed to the engine rather than written anywhere. */
  stageEnvironment?: Record<string, string>
  log: (line: string) => void
}

/**
 * Everything one cloud contributes to a run, as one value.
 *
 * Callers hold this and never ask which cloud produced it. `cloud` and `engine`
 * are here to be reported — a log line, a workflow summary — not to be switched
 * on: a caller that branches on them has put the decision back where this type
 * exists to remove it from.
 */
export type DeployTarget = {
  readonly cloud: Cloud
  readonly engine: Engine
  /** The store this stage's configuration lives in. */
  backend: StoreBackend
  /**
   * Which declared groups this run reads out of that store.
   *
   * A function of the intent and the declaration, because the answer is not one
   * list: a rollout carries each service's own configuration and a teardown has
   * no service to configure, `secret` is only read when the config declares it,
   * and the Pulumi engine needs the passphrase its state is sealed with either
   * way.
   */
  environmentGroups: (intent: Intent, config: Pick<MstageConfig, 'envSelectGroup'>) => readonly string[]
  run: (request: DeployRequest) => Promise<number>
}

export type DeployTargetInput = {
  /** Where the config file is, for the paths the engines resolve against it. */
  config: Pick<MstageConfig, 'root' | 'path'>
  /** `scope.home` is which cloud this stage lives in, folded once by mstage. */
  scope: Scope
  /** Injected so a test drives the dispatch without reaching either cloud. */
  resolveHomeWith?: typeof resolveHome
}

/**
 * The stage's cloud, resolved once, as the bundle that serves it.
 *
 * The switch is on what mstage answered rather than on the config a second
 * time. Narrowing there is also what hands each engine the identity it actually
 * needs — SST a resolved AWS key triple, Pulumi the cloud-neutral questions —
 * with no cast and no runtime check for methods that are always there.
 */
export const resolveDeployTarget = async ({
  config,
  scope,
  resolveHomeWith = resolveHome,
}: DeployTargetInput): Promise<DeployTarget> => {
  const home = await resolveHomeWith({ scope })
  // Exhaustive rather than an if with a fallthrough: a cloud added to mstage and
  // not to this switch should be a type error here, not a run silently handed to
  // whichever engine happened to be the else branch.
  switch (home.identity.home) {
    case 'aws':
      return awsTarget({ config, scope, identity: home.identity, backend: home.backend })
    case 'gcp':
      return gcpTarget({
        config,
        scope,
        identity: home.identity,
        backend: home.backend,
        stateBucket: home.stateBucket,
      })
  }
}
