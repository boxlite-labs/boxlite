/**
 * The stage configuration a deploy runs with.
 *
 * It is fetched from the store and handed to the child process directly. Nothing
 * is written to disk: a file would outlive the deploy, and the places it leaks
 * from — an uploaded artifact, a debug `cat`, a stale working copy — are far
 * easier to get wrong than the IAM that guards the store itself.
 *
 * It is not put into this process's own `process.env` either. `sst` is a child
 * and reads its own environment, so injecting here would only add a second copy
 * visible to anything else in this process, crash dumps included.
 */

import { readEnvironment } from 'mstage/env'
import { SECRET_GROUP, secretAddressesOf } from 'mstage/secret-address'
import { groupKeys, valuesOfGroup } from 'mstage/select-group'
import type { MstageConfig } from 'mstage/config'
import type { StoreBackend } from 'mstage/env'
import type { Scope } from 'mstage/scope'

/** The group `mstage.config.json` must declare for a deploy to fetch anything. */
export const DEPLOY_GROUP = 'deploy'

/**
 * The groups the deploy carries but never reads, one per service.
 *
 * Their values are a running service's, not the deploy's: the deploy's only job
 * with them is to put each where that service will find it at boot. One group
 * per service rather than one shared: a shared group hands every container every
 * secret any of them needs, so a key added for one service silently widens what
 * the others hold.
 *
 * The names are the artifacts `mbuild.config.json` declares, which is what lets
 * a test hold the two files against each other — an artifact with no group, or a
 * group no artifact names, is a service whose secrets nobody delivers.
 */
export const API_GROUP = 'api'
export const PROXY_GROUP = 'proxy'
export const OTEL_GROUP = 'otel-collector'
/**
 * The runner is the one group that is not an image.
 *
 * Every other group is named after an artifact `mbuild.config.json` builds, so
 * a test can hold the two files against each other. The runner breaks that
 * symmetry because BoxLite's runner is a binary on a host with nested KVM, not
 * a container — there is no image for mbuild to build, and its key still has to
 * reach the machine. So the invariant this repository can hold is the one that
 * matters: every artifact has a group. The reverse is not true, and saying so
 * here is cheaper than a test that quietly excluded a name.
 */
export const RUNNER_GROUP = 'runner'
export const SERVICE_GROUPS = [API_GROUP, PROXY_GROUP, OTEL_GROUP, RUNNER_GROUP]

/**
 * The engine's own secret, which no service reads.
 *
 * Pulumi seals the secrets in its state with a passphrase; SST does not, and an
 * AWS stage has no state of that kind to open. Kept out of `deploy` for that
 * reason: a group names keys the store *must* hold, so putting it there would
 * fail every AWS stage over a value it would never read.
 */
export const PULUMI_GROUP = 'pulumi'

/**
 * The names `env.selectGroup` reserves, which name no service.
 *
 * `deploy` is what the deploy itself spends; `secret` marks which keys hold the
 * address of a secret rather than the secret; `pulumi` is what the Pulumi
 * engine opens its state with. Every other group is an artifact
 * `mbuild.config.json` builds, and belongs to that service alone — which is what
 * lets a test hold the two files against each other without the markers
 * confusing the comparison.
 */
export const MARKER_GROUPS = [DEPLOY_GROUP, SECRET_GROUP, PULUMI_GROUP]

/** What a rollout reads: the deploy's own half, and every service's. */
export const ROLLOUT_GROUPS = [DEPLOY_GROUP, ...SERVICE_GROUPS]

export type Environment = Record<string, string | undefined>

/** Where `env.selectGroup` is declared, and what it declares. */
export type GroupDeclaration = {
  groups: Record<string, string[]>
  /**
   * The subset of each group the store need not hold.
   *
   * Carried beside `groups` rather than folded into it, because the two
   * questions are different: "what does this group name" is what the secret
   * marker and the digest ask, and "which of those may be absent" is what a
   * fetch asks. Absent here means every key is required, which is the shape a
   * declaration written as a plain array has.
   */
  optional?: Record<string, string[]>
  where: string
}

/**
 * The secrets one service reads, and no other.
 *
 * Asked for by group name rather than listed here: `mstage.config.json` declares
 * each set once, and a second copy drifts the first time a key is added to one
 * side and not the other. A service gets its own group, so a secret added for
 * one never reaches another's container.
 *
 * Delivery is checked here; content is not. A key the group names but the
 * deploy never received stops the deploy, because a service handed a silently
 * short environment refuses a feature hours later somewhere that does not
 * mention the key. An empty value is delivered as an empty value: the store's
 * own contract counts a key as present without judging it, and whether an empty
 * credential means "disabled" or "misconfigured" is the service's call — the
 * API says which in the `prerequisite` of its 503.
 *
 * These land in the container's plain environment, where anyone who can
 * describe the task definition can read them and where every revision ever
 * registered keeps its own copy. Injecting by reference — an ECS `secrets`
 * entry, a Cloud Run `secretKeyRef` — is what removes both properties, and is
 * why the fix for that is a secret channel on each workload's request rather
 * than more names arriving through this function.
 */
export const serviceSecretsFrom = ({
  group,
  declaration,
  environment,
}: {
  group: string
  declaration: GroupDeclaration
  environment: Environment
}): Record<string, string> => {
  const keys = groupKeys({ group, groups: declaration.groups, where: declaration.where })
  // What the declaration marked as "this stage may not have set it". A feature
  // nobody configured is not a short environment: the service already has an
  // answer for an absent billing origin or an unwired incident.io source.
  const mayBeAbsent = new Set(declaration.optional?.[group] ?? [])
  const undelivered = keys.filter((key) => environment[key] === undefined && !mayBeAbsent.has(key))
  if (undelivered.length > 0) {
    throw new Error(
      `${undelivered.join(', ')} must reach the deploy — env.selectGroup.${group} names ` +
        `${undelivered.length === 1 ? 'it' : 'them'} and ${group} reads ` +
        `${undelivered.length === 1 ? 'it' : 'them'} at boot`,
    )
  }
  return Object.fromEntries(
    keys.filter((key) => environment[key] !== undefined).map((key) => [key, (environment[key] as string).trim()]),
  )
}

/**
 * The same, plus the marker group, where the repository declares one.
 *
 * What tells an address from a value downstream is the declaration, not this
 * list: `splitServiceChannels` reads `env.selectGroup.secret` out of the config
 * it is handed. Asking for the marker here decides something narrower — that a
 * marked key's *value* is fetched at all. A rollout group naming it is what
 * normally brings it, and one that no rollout group named would otherwise be
 * marked and absent.
 *
 * Optional because a repository may deliver every secret as a value, which is
 * what one that declares no `secret` group is saying. Asking for a group that is
 * not declared is a typo everywhere else and would be one here too, so the
 * question is asked once, of the config.
 *
 * The addresses themselves are carried like any other value: an ARN is not a
 * secret, and what makes this delivery worth doing is that the secret it names
 * never travels at all.
 */
export const rolloutGroups = (config: Pick<MstageConfig, 'envSelectGroup'>): string[] =>
  config.envSelectGroup[SECRET_GROUP] ? [...ROLLOUT_GROUPS, SECRET_GROUP] : [...ROLLOUT_GROUPS]

/**
 * An address is delivered to a workload, so the deploy's own group cannot hold
 * one.
 *
 * mstage asks only that some group other than the marker names a marked key,
 * which is as much as a tool that does not know this repository's groups can
 * ask. `deploy` satisfies that and is still the wrong answer: what the deploy
 * spends it reads out of its own process environment, and there is no platform
 * standing between the store and that read to resolve anything. A marked key
 * there would be spent as the text of an address — a Cloudflare token that is
 * an ARN, refused by Cloudflare rather than by anything here.
 *
 * Nor would resolving it be the fix. The point of the channel is that the
 * secret reaches a container without travelling through the deploy; a deploy
 * that read one to spend it would be the case the whole arrangement exists to
 * avoid.
 */
export const assertAddressesAreNotSpent = (config: Pick<MstageConfig, 'envSelectGroup' | 'path'>): void => {
  const marked = config.envSelectGroup[SECRET_GROUP]
  if (!marked) return
  const spent = marked.filter((key) => (config.envSelectGroup[DEPLOY_GROUP] ?? []).includes(key))
  if (spent.length > 0) {
    throw new Error(
      `${config.path}: env.selectGroup.${SECRET_GROUP} marks ${spent.join(', ')}, which ` +
        `env.selectGroup.${DEPLOY_GROUP} also names. An address is resolved by the platform a workload ` +
        'runs on, and the deploy is neither — it spends what that group holds, so a key there holds a value',
    )
  }
}

/**
 * One service's keys, split by how each one travels.
 *
 * `serviceSecretsFrom` above answers "what does this service read"; this answers
 * "and which of those is an address". The marker group is the only thing that
 * decides: nothing about a value's text can be trusted to say it, since an ARN
 * is a perfectly usable plaintext secret.
 *
 * Delivery is already checked by the time this runs, so what is left is the
 * parse — mstage refuses a value that is not an address when it is written, and
 * a store is also writable by `sst secret set`, so the deploy is the second
 * place that has to be sure.
 */
export const splitServiceChannels = ({
  delivered,
  declaration,
  home,
}: {
  delivered: Record<string, string>
  declaration: GroupDeclaration
  home: MstageConfig['home']
}): { values: Record<string, string>; addresses: Record<string, string> } => {
  const marked = new Set(declaration.groups[SECRET_GROUP] ?? [])
  const entries = Object.entries(delivered)
  return {
    values: Object.fromEntries(entries.filter(([key]) => !marked.has(key))),
    addresses: secretAddressesOf({
      values: Object.fromEntries(entries.filter(([key]) => marked.has(key))),
      home,
    }),
  }
}

/**
 * What a teardown reads: what the deploy itself spends, and nothing else.
 *
 * A destroyed stage has no service to configure, so asking for a service group
 * would make a half-configured stage unremovable — the one state most likely to
 * need removing.
 */
export const TEARDOWN_GROUPS = [DEPLOY_GROUP]

export type StageEnvironment = { values: Record<string, string>; source: string }

/**
 * A local deploy skips the fetch and runs on whatever the shell already exports,
 * which is also what CI does today: the workflow puts the values in the job
 * environment before `sst deploy` and never consults the store.
 */
export const ambientEnvironment = (): StageEnvironment => ({
  values: {},
  source: 'the ambient environment',
})

export const fetchStageEnvironment = async ({
  config,
  scope,
  backend,
  groups = ROLLOUT_GROUPS,
  readStore = readEnvironment,
}: {
  config: MstageConfig
  scope: Scope
  /** The store the target resolved, whichever cloud answered. */
  backend: StoreBackend
  /** Which declared groups this run needs. Defaults to everything a rollout reads. */
  groups?: readonly string[]
  /** The store read, injectable so the narrowing can be tested without AWS. */
  readStore?: typeof readEnvironment
}): Promise<StageEnvironment> => {
  const app = scope.app as string
  const stage = scope.stage as string
  // One read narrowed per group, rather than one read each: two reads can
  // straddle an edit, and a deploy that shipped the deploy group from before it
  // and a service group from after would be a stage nobody configured.
  const stored = await readStore({ clients: backend, app, stage })
  const narrow = (group: string): Record<string, string> =>
    valuesOfGroup({
      group,
      groups: config.envSelectGroup,
      values: stored,
      where: config.path,
      optional: config.envOptional[group] ?? [],
    })

  return {
    values: Object.assign({}, ...groups.map(narrow)) as Record<string, string>,
    source: `env.selectGroup.${groups.join(' and .')} of ${app}/${stage}`,
  }
}
