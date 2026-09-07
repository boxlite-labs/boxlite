/**
 * `mstage env` — what a stage's environment holds, and what may leave it.
 *
 * The commands. What a group is and what may leave the store through one lives
 * in `env/select-group.ts`, which every consumer calls — these commands included.
 *
 * Without a group, `list` prints names only. `sst secret list` prints every
 * value with sst's stdio inherited, which was already more than "lists what is
 * set" and became far more once the store started holding whole stage
 * configurations: one command drops every token and private key into scrollback.
 * boxlite replaced that command for the same reason
 * (apps/infra/deployment/secret-names.ts). mstage does not repeat it.
 */

import {
  deleteValues,
  listVersions,
  readEnvironment,
  setValues,
  type Clients,
  type StoreBackend,
  type StoredVersion,
} from '../../env/store.ts'
import { compareDigest, digestOfGroup } from '../../env/digest.ts'
import { assertSecretAddresses } from '../../env/secret-address.ts'
import { ExportError, groupKeys, valuesOfGroup } from '../../env/select-group.ts'
import type { Scope } from '../../aws/precedence.ts'
import type { MstageConfig } from '../../config/load.ts'
import { readRedirect, readValue } from '../prompt.ts'

type Log = (line: string) => void

export const formatEnvironment = ({
  app,
  stage,
  values,
  withValues,
  versionId,
}: {
  app: string
  stage: string
  values: Record<string, string>
  withValues: boolean
  /** Named in the heading, so two versions of one stage are told apart. */
  versionId?: string
}): string[] => {
  // Headed the way `sst secret list` heads it (cmd/sst/secret.go:81-92), and
  // only when populated, so an empty store prints nothing to mistake for one.
  if (Object.keys(values).length === 0) return []
  const lines = [`# ${app}/${stage}${versionId ? ` @ ${versionId}` : ''}`]
  for (const [key, value] of Object.entries(values)) lines.push(withValues ? `${key}=${value}` : key)
  return lines
}

export const list = async ({
  config,
  scope,
  options,
  log,
  backend,
}: {
  config: MstageConfig
  scope: Scope
  options: Record<string, string | boolean>
  log: Log
  /** The store this stage lives in. Resolved once, by `resolveHome`. */
  backend: StoreBackend | Clients
}): Promise<number> => {
  const app = scope.app as string
  const stage = scope.stage as string
  // A deploy records the object version it shipped against, and the service
  // reads that one. This is how you see what it is reading rather than what the
  // store happens to hold now.
  const versionId = typeof options.version === 'string' ? options.version : undefined
  const values = await readEnvironment({
    clients: backend,
    app,
    stage,
    ...(versionId ? { versionId } : {}),
  })

  const group = options['select-group'] as string | undefined
  if (group !== undefined) {
    const selected = valuesOfGroup({
      group,
      groups: config.envSelectGroup,
      values,
      where: config.path,
    })
    // --select-group narrows; it does not reveal. A group can hold a live credential, so
    // printing one stays an explicit act even when the group was named.
    if (options.json === true) log(JSON.stringify(selected, null, 2))
    else if (options.values === true) for (const [key, value] of Object.entries(selected)) log(`${key}=${value}`)
    else {
      for (const key of Object.keys(selected)) log(key)
      log(`# names only; add --values to print them, or --json to export env.selectGroup.${group}`)
    }
    return 0
  }

  if (options.json === true) {
    log(JSON.stringify(values, null, 2))
    return 0
  }

  const withValues = options.values === true
  const lines = formatEnvironment({ app, stage, values, withValues, ...(versionId ? { versionId } : {}) })
  // SST treats an empty store as a failure rather than as an empty answer, and a
  // caller that cannot tell the two apart would deploy with no configuration.
  if (lines.length === 0) throw new Error(`No secrets found for ${app}/${stage}`)
  for (const line of lines) log(line)
  if (!withValues) log('# names only; add --values to print them, or --select-group=<group> to export one')
  return 0
}

/** Splits on the first `=`, so a value may contain more of them. */
const splitAssignment = (assignment: string): [string, string] => {
  const separator = assignment.indexOf('=')
  if (separator <= 0) throw new Error(`"${assignment}" is not a KEY=VALUE assignment`)
  return [assignment.slice(0, separator), assignment.slice(separator + 1)]
}

/**
 * `KEY=VALUE`, where the value is a line of text.
 *
 * Escapes are expanded because a shell has no way to put a newline in an
 * argument without them, and the same sequences SST's own file loader accepts
 * (cmd/sst/secret.go:199-207) are the ones expanded here. A value that needs a
 * literal backslash-n writes `\\n`; a value too awkward for either goes through
 * stdin instead.
 */
export const parseAssignment = (assignment: string): [string, string] => {
  const [name, value] = splitAssignment(assignment)
  return [name, unescape(value)]
}

/**
 * `KEY=VALUE`, where the value is a JSON document. The `--json` form.
 *
 * Asked for rather than detected. A value that begins with `{` is far more
 * often a document than not, but "far more often" is the wrong footing for a
 * store this one writes to: it would make `KEY={VALUE}` a refusal instead of the
 * two words it says, and the caller who meant a document is the one who can say
 * so in a word.
 *
 * What the flag buys is the parse. The document is refused if it does not parse
 * — a mistyped one lands nowhere rather than being found later by whatever reads
 * the key — and it is stored as JSON writes it rather than as the shell typed
 * it, so one value is one stored string: re-typing the same object with other
 * spacing is not a change and does not move a group's digest.
 *
 * The expansion above does not run on it, because a document carries its own
 * escapes: `{"pem":"a\nb"}` already means a newline, and expanding it first
 * would leave a raw newline inside a JSON string, which is not JSON at all.
 */
export const parseJsonAssignment = (assignment: string): [string, string] => {
  const [name, value] = splitAssignment(assignment)
  return [name, jsonDocument(name, value)]
}

/**
 * One JSON document, as JSON writes it.
 *
 * The parser's own message is not repeated: Node quotes the offending input in
 * it, and the input here is a value that was typed into a store because it does
 * not belong in a terminal.
 */
const jsonDocument = (name: string, value: string): string => {
  let parsed: unknown
  try {
    parsed = JSON.parse(value) as unknown
  } catch {
    throw new Error(
      `${name} was given with --json, so its value must be a JSON document, and it does not parse. ` +
        `Quote it whole for the shell — ${name}='{"a":"b"}' — or drop --json to store it as text`,
    )
  }
  return storedFrom(name, parsed)
}

/**
 * What one parsed JSON value is stored as.
 *
 * Shared by both ways a document arrives, because they are the same document. A
 * top-level string is stored as parsed rather than with its quotes, so `--json
 * KEY='"a"'` and a piped `{"KEY": "a"}` put the same three characters in the
 * store; anything else is stored as JSON writes it. `null` is refused either
 * way, for the same reason: a key whose stored value is the text `null` is
 * nobody's intention, and a key that should not be there at all is `env del`.
 *
 * Written once because the alternative was two rules that agreed for objects
 * and disagreed for a string — the shape most likely to be typed by hand and
 * least likely to be noticed when it comes back quoted.
 */
const storedFrom = (name: string, parsed: unknown): string => {
  if (parsed === null) throw new Error(`${name} is null; remove a key with env del, or give it a value`)
  return typeof parsed === 'string' ? parsed : JSON.stringify(parsed)
}

const ESCAPES: Record<string, string> = { n: '\n', r: '\r', t: '\t', '\\': '\\', '"': '"' }

export const unescape = (value: string): string =>
  value.replace(/\\(.)/g, (whole, character: string) => ESCAPES[character] ?? whole)

/**
 * A whole store's worth of assignments, read as JSON.
 *
 * Each value goes through `storedFrom`, which is also what `--json` puts one
 * value on a command line through, so a document says the same thing whichever
 * way it arrives and a nested object needs no escaping to survive being written
 * into a file. `{"K": {"a": "b"}}` stores `{"a":"b"}`; `{"K": ["a", "b"]}`
 * stores `["a","b"]`, verbatim rather than joined on a separator its own
 * elements may contain; `{"K": 8080}` stores `8080`; `{"K": null}` is refused.
 *
 * What `env list --json` prints is still what loads back in unedited: it prints
 * every value as the string it is, and a string is what this stores. A document
 * written by the *older* `env list --json` is the case worth naming, and the
 * second return value names it: that version split a value holding a comma into
 * an array, so an array here may be a list somebody meant or the pieces of a
 * value that never was one. Both store as the text of the array, and silence
 * about it is what turned the tool's own documented round trip into a quiet
 * rewrite. An object or a number is not named — no version of `env list --json`
 * ever produced one, so it can only be what its author wrote.
 *
 * JSON is the only accepted document format because it is the only one that
 * carries a newline without an escape convention to learn — and for that reason
 * values are used exactly as JSON parsed them, with no second pass of
 * `unescape`.
 */
export const parseBatch = (text: string): { entries: [string, string][]; converted: string[] } => {
  let parsed: unknown
  try {
    parsed = JSON.parse(text)
  } catch (error) {
    throw new Error(`stdin does not hold valid JSON: ${(error as Error).message}`)
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('stdin must hold a JSON object of names to values, as `env list --json` prints one')
  }
  const converted: string[] = []
  const entries = Object.entries(parsed).map(([name, value]): [string, string] => {
    // Only a list is named. The older export could produce one where a value
    // had been; an object or a number is what its author wrote either way.
    if (Array.isArray(value)) converted.push(name)
    return [name, storedFrom(name, value)]
  })
  return { entries, converted }
}

/**
 * Sets keys. No value is ever echoed: the whole point of the store is that these
 * do not belong in a terminal, and a confirmation that repeats what was just
 * typed puts it in scrollback anyway. What is reported is names and what
 * happened to them.
 *
 * Three ways to say what to set, because they answer different problems: `KEY=V`
 * arguments for a value that fits on a line, a lone `KEY` for one that has to
 * come off a file, and a piped JSON object for a whole stage at once. `--json`
 * says the value is a JSON document — for an argument and for the file the lone
 * `KEY` form reads alike, which is the form a document too large for a command
 * line arrives in.
 */
export const set = async ({
  config,
  scope,
  positionals,
  options,
  log,
  backend,
  readInput = readValue,
  readBatch = readRedirect,
}: {
  config: MstageConfig
  scope: Scope
  positionals: string[]
  options: Record<string, string | boolean>
  log: Log
  /** The store this stage lives in. Resolved once, by `resolveHome`. */
  backend: StoreBackend | Clients
  readInput?: () => Promise<string>
  readBatch?: () => Promise<string>
}): Promise<number> => {
  // A repository that does not fingerprint its configuration has no key to write
  // one to, so --digest asks for nothing and the assignments still land. Said out
  // loud rather than skipped quietly, because the caller asked for it.
  const digest = options.digest === true ? config.envDigest : null

  // A lone name with no `=` reads its value from stdin, so a multi-line one never
  // has to survive a shell's quoting. Only alone: with several arguments there is
  // one stdin and no way to say which of them it belongs to.
  const fromStdin = positionals.length === 1 && !positionals[0]!.includes('=')
  // With no arguments at all, stdin is a whole document rather than one value.
  // An empty read means nothing was piped, which leaves --digest as the only
  // thing this invocation could have meant.
  //
  // Except when --digest was asked for: that is already a complete request, so
  // nothing waits on stdin for it. A pipe nobody writes to never closes, and a
  // step that recomputes a fingerprint would hang rather than finish — which is
  // exactly what `env set --digest` did the first time it ran in a shell whose
  // stdin was inherited.
  //
  // It also keeps one invocation from both rewriting a value and certifying the
  // rewrite: a document's lists become the text of a list, and the fingerprint
  // that would vouch for them is never computed in the same breath. Doing both
  // takes two commands, deliberately.
  const expectsBatch = positionals.length === 0 && options.digest !== true
  const batch = expectsBatch ? (await readBatch()).trim() : ''

  // `--json` describes the values this line carries, so it has nothing to say
  // about a piped document that is already one, or about a line that carries no
  // value at all. Said rather than ignored: a flag that quietly does nothing
  // reads as a flag that did something. Before the two checks below, because it
  // is the more specific complaint about the same invocation.
  const asJson = options.json === true
  if (asJson && positionals.length === 0) {
    throw new Error(
      batch
        ? 'a piped document is already JSON; --json says that a KEY=VALUE value is a JSON document'
        : '--json says that a KEY=VALUE value is a JSON document; this line gives none',
    )
  }

  if (positionals.length === 0 && batch === '') {
    if (options.digest !== true) {
      throw new Error(
        'usage: npm run mstage env set -- KEY=VALUE [KEY=VALUE …] --stage=<stage>, ' +
          'or pipe a JSON object of them, or --digest alone',
      )
    }
    // `--digest` on its own rewrites the fingerprint over the store as it already
    // stands, which is how a group edited by other means gets certified again.
    if (!digest) {
      throw new Error(`${config.path} declares no env.digest, so --digest alone would write nothing`)
    }
  }

  let given: [string, string][]
  let converted: string[] = []
  if (batch) {
    ;({ entries: given, converted } = parseBatch(batch))
  } else if (fromStdin) {
    // Stored as it came off the file — trailing newline and all — unless it was
    // asked for as a document, which is read the way an argument's value is.
    const name = positionals[0]!
    const value = await readInput()
    given = [[name, asJson ? jsonDocument(name, value) : value]]
  } else {
    given = positionals.map(asJson ? parseJsonAssignment : parseAssignment)
  }

  const duplicates = given.map(([name]) => name).filter((name, index, all) => all.indexOf(name) !== index)
  if (duplicates.length > 0) {
    throw new Error(`${[...new Set(duplicates)].join(', ')} given more than once; the last one would silently win`)
  }

  // `--select-group` narrows a document to the keys one group names, so a whole store
  // exported from another stage can be piped in and only the reviewed part of it
  // lands. What it drops is named, because a key that vanishes silently looks
  // like a key that was written.
  const group = options['select-group'] as string | undefined
  const wanted = group === undefined ? null : groupKeys({ group, groups: config.envSelectGroup, where: config.path })
  const entries = wanted === null ? given : given.filter(([name]) => wanted.includes(name))
  const ignored = wanted === null ? [] : given.filter(([name]) => !wanted.includes(name))
  if (ignored.length > 0)
    log(`# outside env.selectGroup.${group}, not set: ${ignored.map(([name]) => name).join(', ')}`)
  if (wanted !== null && given.length > 0 && entries.length === 0) {
    throw new Error(
      `none of the ${given.length} keys given are in env.selectGroup.${group}, so there is nothing to set`,
    )
  }

  // Named, never silent. A list in a piped document may be a list somebody meant
  // or the pieces an older `env list --json` cut a comma-holding value into, and
  // this stores both as the text of the list — a rewrite, in the second case, of
  // a file the tool's own documented round trip produced.
  const lists = converted.filter((name) => entries.some(([key]) => key === name))
  if (lists.length > 0) {
    log(
      `# stored as the JSON text of a list: ${lists.join(', ')}. An export written before mstage ` +
        'dropped its comma convention holds arrays where the store held one comma-joined value',
    )
  }

  // A key in the secret group holds an address, so the secret itself is refused
  // there: the store would take it, and every task would then fail to start
  // trying to resolve a secret as if it were the name of one.
  assertSecretAddresses({ entries, groups: config.envSelectGroup, home: config.home })

  if (scope.protect && options.confirm !== true) {
    throw new Error(`Stage "${scope.stage}" is protected; add --confirm to write to it`)
  }

  if (options.digest === true && !digest) {
    log(`# ${config.path} declares no env.digest; nothing to recompute`)
  }

  const app = scope.app as string
  const stage = scope.stage as string
  const { outcomes } = await setValues({
    clients: backend,
    app,
    stage,
    entries,
    // Computed from the store as it will be, not as it was: a digest of the
    // previous configuration would certify the wrong thing.
    ...(digest
      ? {
          derive: (values: Record<string, string>) => [
            [
              digest.key,
              digestOfGroup({
                values: valuesOfGroup({
                  group: digest.group,
                  groups: config.envSelectGroup,
                  values,
                  where: config.path,
                  // The one member this write is producing, so a store that has
                  // never held a digest can still be given its first one.
                  optional: [digest.key],
                }),
                digestKey: digest.key,
              }),
            ] as [string, string],
          ],
        }
      : {}),
  })

  for (const { name, existed, unchanged } of outcomes) {
    log(`${name} ${unchanged ? 'already set to that value' : existed ? 'replaced' : 'added'} in ${app}/${stage}`)
  }
  if (outcomes.some((outcome) => !outcome.unchanged)) {
    log('# run a deploy for the change to reach anything running')
  }
  return 0
}

const COLUMNS = ['VERSION ID', 'TYPE', 'LAST MODIFIED', 'SIZE', 'STORAGE CLASS'] as const

/** One row per version, in the order S3's own console shows these fields. */
export const formatVersions = (versions: StoredVersion[]): string[] => {
  const rows = versions.map((version) => [
    version.versionId,
    version.type,
    version.lastModified?.toISOString() ?? '-',
    version.size === null ? '-' : String(version.size),
    version.storageClass ?? '-',
  ])
  const widths = COLUMNS.map((column, index) => Math.max(column.length, ...rows.map((row) => row[index]!.length), 0))
  // The last column is not padded, so nothing trails a line with spaces.
  const line = (cells: readonly string[]) =>
    cells.map((cell, index) => (index === cells.length - 1 ? cell : cell.padEnd(widths[index]!))).join('  ')
  return [line(COLUMNS), ...rows.map(line)]
}

/**
 * `mstage env versions` — what the store has held for this stage.
 *
 * The version ids here are what `--version` reads and what a deploy pins, so
 * this is how you find the one a running service was built against, and what
 * changed since.
 */
export const versions = async ({
  scope,
  log,
  backend,
}: {
  scope: Scope
  log: Log
  /** The store this stage lives in. Resolved once, by `resolveHome`. */
  backend: StoreBackend | Clients
}): Promise<number> => {
  const app = scope.app as string
  const stage = scope.stage as string
  const found = await listVersions({ clients: backend, app, stage })
  if (found.length === 0) {
    // Either the stage was never written or the bucket keeps no versions. Both
    // are answers, and neither is an error.
    log(`# ${app}/${stage} has no stored versions`)
    return 0
  }
  log(`# ${app}/${stage}`)
  for (const line of formatVersions(found)) log(line)
  return 0
}

/**
 * `mstage env digest` — does the stored fingerprint still describe the group?
 *
 * Recomputes over the same keys `--digest` would have written and compares. A
 * mismatch means the configuration moved after something was built against it.
 */
export const digest = async ({
  config,
  scope,
  log,
  backend,
}: {
  config: MstageConfig
  scope: Scope
  log: Log
  /** The store this stage lives in. Resolved once, by `resolveHome`. */
  backend: StoreBackend | Clients
}): Promise<number> => {
  const declared = config.envDigest
  if (!declared) throw new ExportError(`${config.path} declares no env.digest`)

  const values = await readEnvironment({
    clients: backend,
    app: scope.app as string,
    stage: scope.stage as string,
  })
  const group = valuesOfGroup({
    group: declared.group,
    groups: config.envSelectGroup,
    values,
    where: config.path,
  })
  const comparison = compareDigest({ values: group, digestKey: declared.key })

  log(`expect: ${comparison.expected}`)
  log(`got:    ${comparison.stored ?? '(not set)'}`)
  if (comparison.matches) return 0
  log(`# ${declared.key} does not describe env.selectGroup.${declared.group} of ${scope.app}/${scope.stage}`)
  return 1
}

/**
 * Removes keys. One that was not there is reported and not an error: the store
 * ends up in the state that was asked for either way, and a caller cleaning up
 * after a rename should not have to know which half already ran.
 *
 * Several names land in one write, for the reason `set` does: the store is a
 * single object, so removing them one at a time would cost a round trip each and
 * widen the window in which a concurrent writer loses somebody's change. A name
 * repeated on the line is refused rather than reported twice for one removal —
 * in a list of keys long enough to want this command, a repeat is a slip worth
 * naming.
 *
 * `--digest` keeps the fingerprint true, which for a removal means refusing one
 * that would falsify it. A group member that goes takes the truth of the stored
 * digest with it, and no recomputation makes it true again while the group still
 * names that member — so the flag checks rather than writes, and says what it
 * checked. `set --digest` is the half that writes.
 */
export const del = async ({
  config,
  scope,
  positionals,
  options,
  log,
  backend,
}: {
  config: MstageConfig
  scope: Scope
  positionals: string[]
  options: Record<string, string | boolean>
  log: Log
  /** The store this stage lives in. Resolved once, by `resolveHome`. */
  backend: StoreBackend | Clients
}): Promise<number> => {
  if (positionals.length === 0) {
    throw new Error('usage: npm run mstage env del -- KEY [KEY …] --stage=<stage>')
  }
  // No naming rule beyond this one: a store mstage can open holds whatever is in
  // it, and refusing to remove a key over how it is spelled would leave it
  // there. An empty name is the exception — no store holds one, so it is a
  // shell artefact rather than a key, and reporting it as "was not set" would
  // be answering a question nobody asked.
  if (positionals.some((name) => name === '')) {
    throw new Error('an empty name is not a key; every name given must be one')
  }
  const duplicates = positionals.filter((name, index) => positionals.indexOf(name) !== index)
  if (duplicates.length > 0) {
    throw new Error(`${[...new Set(duplicates)].join(', ')} given more than once; one removal is enough`)
  }

  // A repository that does not fingerprint its configuration has nothing for the
  // flag to protect, and the removals still land. Said out loud rather than
  // skipped quietly, because the caller asked for it.
  const fingerprint = options.digest === true ? config.envDigest : null
  if (options.digest === true && !fingerprint) {
    log(`# ${config.path} declares no env.digest; there is no fingerprint to keep true`)
  }
  if (fingerprint) {
    const certified = groupKeys({ group: fingerprint.group, groups: config.envSelectGroup, where: config.path })
    const inside = positionals.filter((name) => certified.includes(name))
    if (inside.length > 0) {
      throw new Error(
        `--digest keeps ${fingerprint.key} true, and env.selectGroup.${fingerprint.group} names ` +
          `${inside.join(', ')}: removing those falsifies it, and no recomputation mends that while the ` +
          'group still names them. Nothing was removed',
      )
    }
  }

  if (scope.protect && options.confirm !== true) {
    throw new Error(`Stage "${scope.stage}" is protected; add --confirm to write to it`)
  }

  const app = scope.app as string
  const stage = scope.stage as string
  const { outcomes } = await deleteValues({ clients: backend, app, stage, names: positionals })
  for (const { name, existed } of outcomes) {
    log(`${name} ${existed ? 'removed from' : 'was not set in'} ${app}/${stage}`)
  }
  // What the flag verified, said only when a removal actually happened: a line
  // reassuring about a store that did not move is noise.
  if (fingerprint && outcomes.some((outcome) => outcome.existed)) {
    log(`# none of these is in env.selectGroup.${fingerprint.group}, so ${fingerprint.key} still describes it`)
  }
  if (outcomes.some((outcome) => outcome.existed)) {
    log('# run a deploy for the change to reach anything running')
  }
  return 0
}
