/**
 * Asking the operator a yes/no question.
 *
 * Every sign-in mstage can start opens a browser and waits for a callback, so it
 * cannot complete unattended. Guiding someone through one is therefore only
 * offered where there is someone to guide: CI supplies credentials through OIDC
 * and job environment instead, and a prompt there would hang the job rather
 * than fail it.
 */

import { createInterface } from 'node:readline/promises'

export type Confirm = (question: string) => Promise<boolean>

export const isInteractive = (stream: { isTTY?: boolean } = process.stdin): boolean => Boolean(stream.isTTY)

export const confirm: Confirm = async (question) => {
  const rl = createInterface({ input: process.stdin, output: process.stdout })
  try {
    return /^y(es)?$/i.test((await rl.question(question)).trim())
  } finally {
    rl.close()
  }
}

/**
 * The value for `env set KEY=` when the assignment leaves it empty.
 *
 * Matches `sst secret set` (cmd/sst/secret.go:335-362): a terminal is prompted
 * for one line with its newline stripped, and a redirect is read whole. A file's
 * trailing newline is kept because that is what SST stores, and the two write to
 * the same object — the same PEM must not differ depending on which tool set it.
 */
export const readValue = async (stream: NodeJS.ReadStream = process.stdin): Promise<string> => {
  if (stream.isTTY) {
    const rl = createInterface({ input: stream, output: process.stdout })
    try {
      return await rl.question('Enter value: ')
    } finally {
      rl.close()
    }
  }
  let text = ''
  for await (const chunk of stream) text += chunk
  return text
}

/**
 * Whatever a redirect carries, or nothing when there is no redirect.
 *
 * `readValue` prompts a terminal because it knows a value is wanted. A batch is
 * different: `env set` reads one only when a document was piped in, so a
 * terminal must answer "nothing" rather than sit waiting for a JSON object the
 * caller never meant to type.
 */
export const readRedirect = async (stream: NodeJS.ReadStream = process.stdin): Promise<string> => {
  if (stream.isTTY) return ''
  let text = ''
  for await (const chunk of stream) text += chunk
  return text
}
