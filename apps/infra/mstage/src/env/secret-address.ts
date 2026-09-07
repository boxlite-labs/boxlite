/*
 * A secret a workload is handed by reference rather than by value.
 *
 * The store holds the address, never the secret: `{"address": "arn:…"}` for a
 * Parameter Store SecureString, `{"address": "projects/…/secrets/…"}` for a
 * Secret Manager secret. What resolves it is the platform the workload runs on —
 * an ECS `secrets` entry, a Cloud Run `secretKeyRef` — so the value never enters
 * a task definition, a revision that keeps its own copy forever, or the deploy
 * that arranged it.
 *
 * `env.selectGroup.secret` is what says which keys are addresses. Nothing about
 * a value's text can be trusted to say it: an ARN is a perfectly usable
 * plaintext secret, and a plaintext secret that happens to begin with `arn:`
 * would be handed to a container as if it were an address. One declaration
 * decides, reviewed where every other group is.
 *
 * The address format is the cloud's, so `home` decides which one is accepted.
 * Both AWS forms are, because both are what that reference channel resolves and
 * this platform's existing secrets live in Secrets Manager
 * (`apps/infra/README.md`); which of the two services holds a secret is not a
 * question the store has any business answering.
 *
 * No message here ever quotes a value. The one mistake this module exists to
 * catch is a plaintext secret written where an address belongs, and a refusal
 * that echoed it would put it in the terminal the write was trying to keep it
 * out of.
 */

import { EnvError } from './backend.ts'
import type { MstageConfig } from '../config/load.ts'

/**
 * The one group whose values are addresses. Named here because this is the
 * module that gives the name its meaning; `config/load.ts` reads it to refuse a
 * key that some other group also names.
 */
export const SECRET_GROUP = 'secret'

/** The only field a stored address has. Anything else is a typo, not an option. */
const ADDRESS_FIELD = 'address'

type AddressForm = { pattern: RegExp; describe: string }

/**
 * What each cloud's reference channel can resolve.
 *
 * A full ARN rather than a bare parameter name: ECS accepts a bare name only
 * for a parameter in the task's own region and account, and an ARN is the form
 * a reviewer can read the region and the account out of. Likewise the Secret
 * Manager form is the resource name and not a version — Cloud Run takes the
 * version as its own field, so an address carrying one would be declaring it
 * twice.
 */
const ADDRESS_FORMS: Record<MstageConfig['home'], AddressForm> = {
  aws: {
    pattern: /^arn:aws[a-z0-9-]*:(?:ssm:[a-z0-9-]+:\d{12}:parameter\/|secretsmanager:[a-z0-9-]+:\d{12}:secret:)\S+$/,
    describe:
      'a Parameter Store parameter ARN (arn:aws:ssm:<region>:<account>:parameter/<name>) ' +
      'or a Secrets Manager secret ARN',
  },
  gcp: {
    pattern: /^projects\/[a-z0-9-]+\/secrets\/[A-Za-z0-9_-]+$/,
    describe: 'a Secret Manager secret name (projects/<project>/secrets/<secret>), with no version on the end',
  },
}

/** The address one stored value holds, or an error naming the key and the form. */
const addressOf = ({ key, value, home }: { key: string; value: string; home: MstageConfig['home'] }): string => {
  // Every home has an entry: `config/load.ts` refuses a `home` that is not one
  // of these two, so there is no third case to answer for here.
  const form = ADDRESS_FORMS[home]
  const expected = `a key in env.selectGroup.${SECRET_GROUP} holds {"${ADDRESS_FIELD}": …}, naming ${form.describe}`

  let parsed: unknown
  try {
    parsed = JSON.parse(value)
  } catch {
    // Deliberately without the parser's own message: Node quotes the input in
    // it, which for this key is the thing that must not be quoted.
    throw new EnvError(`${key} does not hold JSON, and ${expected}`)
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new EnvError(`${key} does not hold a JSON object, and ${expected}`)
  }

  const extra = Object.keys(parsed).filter((field) => field !== ADDRESS_FIELD)
  if (extra.length > 0) {
    throw new EnvError(`${key} names ${extra.join(', ')}, which an address has no field for; ${expected}`)
  }

  const address = (parsed as Record<string, unknown>)[ADDRESS_FIELD]
  if (typeof address !== 'string' || address.trim() === '') {
    throw new EnvError(`${key} has no "${ADDRESS_FIELD}"; ${expected}`)
  }
  if (!form.pattern.test(address)) throw new EnvError(`${key} does not name ${form.describe}`)
  return address
}

/**
 * Refuses a value a key in the secret group cannot hold, before anything is
 * written.
 *
 * The mistake worth stopping here is writing the secret itself where its
 * address belongs: the store would take it, the deploy would hand it to a
 * container as an address, and every task would fail to start for a reason that
 * names neither the key nor the write. Keys outside the group are not checked,
 * and a repository that declares no such group has nothing to check.
 */
export const assertSecretAddresses = ({
  entries,
  groups,
  home,
}: {
  entries: readonly [string, string][]
  groups: Record<string, string[]>
  home: MstageConfig['home']
}): void => {
  const declared = groups[SECRET_GROUP]
  if (!declared) return
  for (const [key, value] of entries) if (declared.includes(key)) addressOf({ key, value, home })
}

/**
 * One group's values, read as the addresses they are.
 *
 * Handed the group already narrowed — `valuesOfGroup` is what says a group must
 * be complete, and this has no second opinion about it. What it adds is the one
 * thing only this module knows: that each of these values is an address, and
 * which shapes this cloud can resolve.
 */
export const secretAddressesOf = ({
  values,
  home,
}: {
  values: Record<string, string>
  home: MstageConfig['home']
}): Record<string, string> =>
  Object.fromEntries(Object.entries(values).map(([key, value]) => [key, addressOf({ key, value, home })]))
