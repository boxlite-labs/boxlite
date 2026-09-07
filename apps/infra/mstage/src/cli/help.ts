/**
 * What each module and command accepts.
 *
 * Kept beside the registry rather than in a README because the shape of an
 * invocation is the one thing a caller cannot guess: npm eats options written
 * left of the `--`, so a usage line that omits the separator produces a command
 * nobody typed. Every line printed here is copy-pasteable as written.
 */

import { FLAG_OPTIONS, SHORT_ALIASES } from './argv.ts'

export const OPTION_HELP: Record<string, string> = {
  stage: 'which stage to act on; must be declared in mstage.config.json',
  app: 'which app owns the store, when it is not the one mstage.config.json names',
  region: 'the AWS region to act in, overriding whatever else resolves it',
  'role-arn': 'assume this role on top of the resolved credentials',
  'role-session-name': 'name that session in CloudTrail (default: mstage)',
  confirm: 'proceed on a stage mstage.config.json marks protected',
  force: 'sign in again first, instead of only reporting the current session',
  logout: 'end the current session instead of checking it',
  values: 'print values, not only names',
  'select-group': 'one env.selectGroup from mstage.config.json: list exports it, set writes only its keys',
  json: 'JSON: list emits a group as JSON for another tool; set reads the value it is given as a JSON document',
  version: 'read the object version a deploy pinned, instead of the current one',
  digest: 'keep env.digest.key true: set writes it, del refuses a removal that would falsify it',
  help: 'print this',
}

/** Short stand-ins, so a long option name does not become a long placeholder. */
const PLACEHOLDER: Record<string, string> = { 'role-arn': 'arn', 'role-session-name': 'name', 'select-group': 'group' }

export type CommandSpec = {
  /** Absent for a module that dispatches its commands itself, as login does. */
  run?: (input: any) => Promise<unknown>
  summary: string
  inner?: 'required'
  /**
   * A positional the command takes, such as `env set KEY=VALUE`. `optional`
   * states when it may be left out, for a command that can be given its work
   * some other way; the dispatcher then leaves the check to the command.
   */
  argument?: { form: string; description: string; optional?: string }
  requires?: string[]
  accepts?: string[]
}

export type ModuleSpec = {
  summary: string
  scope: 'login' | 'stage'
  commands?: Record<string, CommandSpec>
  /** Said after the command list, where what the list means needs qualifying. */
  commandNote?: string
  /** Options every command in the module takes, listed once rather than per command. */
  accepts?: string[]
  /** A real invocation. Written out per module because a generic one is often wrong. */
  example: string
}

const INNER_FORM = '-- <command> [args…]'

// The `=` form is shown because it survives every position; the spaced form only
// works right of the separator.
const SHORT_FOR: Record<string, string> = Object.fromEntries(
  Object.entries(SHORT_ALIASES).map(([short, long]) => [long, short]),
)

const form = (name: string): string => {
  const long = FLAG_OPTIONS.includes(name) ? `--${name}` : `--${name}=<${PLACEHOLDER[name] ?? name}>`
  return SHORT_FOR[name] ? `-${SHORT_FOR[name]}, ${long}` : long
}

const describe = (name: string, required: boolean): [string, string] => [
  form(name),
  `${required ? 'required · ' : ''}${OPTION_HELP[name] ?? ''}`,
]

export const moduleUsage = (name: string, module: ModuleSpec): string => {
  const commands = Object.entries(module.commands ?? {})
  const rows: [string, string][] = []
  for (const [, spec] of commands) {
    for (const option of spec.requires ?? []) rows.push(describe(option, true))
    for (const option of spec.accepts ?? []) rows.push(describe(option, false))
  }
  for (const option of module.accepts ?? []) rows.push(describe(option, false))
  if (commands.some(([, spec]) => spec.inner === 'required')) rows.push([INNER_FORM, ''])
  for (const [, spec] of commands) if (spec.argument) rows.push([spec.argument.form, ''])
  const width = Math.max(0, ...rows.map(([left]) => left.length)) + 2

  // From the content, so a command as long as the column does not run into
  // its own summary.
  const commandWidth = Math.max(0, ...commands.map(([command]) => command.length)) + 2

  const lines = [`usage: npm run mstage ${name} <command> -- [options]`, '']

  for (const [command, spec] of commands) {
    lines.push(`  ${command.padEnd(commandWidth)}${spec.summary}`)
    if (spec.argument) {
      const need = spec.argument.optional ? `required unless ${spec.argument.optional}` : 'required'
      lines.push(`      ${spec.argument.form.padEnd(width)}${need} · ${spec.argument.description}`)
    }
    for (const option of spec.requires ?? []) {
      const [left, right] = describe(option, true)
      lines.push(`      ${left.padEnd(width)}${right}`)
    }
    for (const option of spec.accepts ?? []) {
      const [left, right] = describe(option, false)
      lines.push(`      ${left.padEnd(width)}${right}`)
    }
    if (spec.inner === 'required') {
      lines.push(`      ${INNER_FORM.padEnd(width)}required · the command to run under that identity`)
    }
  }

  // After the list, because it qualifies what the list means.
  if (module.commandNote) lines.push('', module.commandNote)

  if (module.accepts?.length) {
    lines.push('', commands.length > 0 ? '  every command also takes' : '  options')
    for (const option of module.accepts) {
      const [left, right] = describe(option, false)
      lines.push(`      ${left.padEnd(width)}${right}`)
    }
  }

  lines.push('', 'every option must sit to the right of the "--"; npm claims anything left of it')
  lines.push(`example: ${module.example}`)
  return lines.join('\n')
}
