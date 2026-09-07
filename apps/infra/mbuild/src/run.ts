/*
 * One external command, plus whatever it reads on stdin.
 *
 * stdin is always a pipe and always closed, rather than piped only when there
 * is something to send: a literal stdio tuple is what lets `spawn` type stdout
 * and stderr as streams instead of `null`, and a command with nothing to read
 * sees the same end of input either way.
 *
 * `options.echo` decides when a command's output becomes visible, never whether
 * it is returned: the result carries stdout and stderr either way, so a caller
 * that echoes a command can still read what it printed.
 */

import { spawn } from 'node:child_process'
import type { RunOptions, RunResult } from './publish.ts'

/** Progress, so stderr — stdout carries what the command was run to produce. */
const toStandardError = (chunk: string): void => void process.stderr.write(chunk)

/**
 * `echoTo` is where this process's log goes, injected for the same reason
 * `publish` takes its `log`: nothing else here can be observed without it.
 */
export const run = (
  command: string,
  args: string[],
  options: RunOptions = {},
  echoTo: (chunk: string) => void = toStandardError,
): Promise<RunResult> =>
  new Promise((resolve, reject) => {
    const child = spawn(command, args, { stdio: ['pipe', 'pipe', 'pipe'] })
    let stdout = ''
    let stderr = ''
    // Decoded per stream rather than per chunk: a docker build writes UTF-8,
    // and a multi-byte character split across two reads would reach the log as
    // two broken ones.
    child.stdout.setEncoding('utf8')
    child.stderr.setEncoding('utf8')
    // Echoed as it arrives and as it was written: docker's own carriage returns
    // and indentation are what make its progress readable.
    child.stdout.on('data', (chunk: string) => {
      if (options.echo) echoTo(chunk)
      stdout += chunk
    })
    child.stderr.on('data', (chunk: string) => {
      if (options.echo) echoTo(chunk)
      stderr += chunk
    })
    child.on('error', reject)
    child.on('close', (code) => resolve({ code: code ?? 1, stdout, stderr }))
    // A child that exits before reading breaks the pipe. Its exit code is the
    // failure worth reporting, and an unhandled stream error would take this
    // process down before it could be.
    child.stdin.on('error', () => {})
    child.stdin.end(options.stdin ?? '')
  })
