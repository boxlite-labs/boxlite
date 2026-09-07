import assert from 'node:assert/strict'
import test from 'node:test'
import { run } from '../src/run.ts'

/**
 * A command that says something and then keeps working, which is the shape of
 * every docker build: minutes of output before an exit code.
 */
const SAYS_ONE_THING_THEN_WORKS = [
  'process.stderr.write("step 1/2\\n")',
  'setTimeout(() => process.stderr.write("step 2/2\\n"), 500)',
].join(';')

test('an echoed command reaches the log while it runs, not once it has exited', async () => {
  // The failure this guards is a step whose log stays empty for ten minutes: a
  // build that prints nothing cannot be told from a build that has hung.
  const echoed: string[] = []
  let announce = (_chunk: string) => {}
  const firstEcho = new Promise<string>((resolve) => {
    announce = resolve
  })
  const running = run('node', ['-e', SAYS_ONE_THING_THEN_WORKS], { echo: true }, (chunk) => {
    echoed.push(chunk)
    announce(chunk)
  })

  assert.equal(await Promise.race([firstEcho, running.then(() => 'the command exited first')]), 'step 1/2\n')

  const result = await running
  assert.equal(echoed.join(''), 'step 1/2\nstep 2/2\n', 'the log gets what the command wrote, unchanged')
  assert.equal(result.stderr, echoed.join(''), 'echoing is when the output is visible, not whether it is returned')
})

test('a command whose output is a value to read is not echoed', async () => {
  // `aws ecr get-login-password` prints a registry password, and `describe-*`
  // prints JSON to parse. Echoing by default would put the first in the log.
  const echoed: string[] = []
  const result = await run('node', ['-e', 'process.stdout.write("registry-password")'], {}, (chunk) =>
    echoed.push(chunk),
  )

  assert.equal(result.stdout, 'registry-password')
  assert.deepEqual(echoed, [])
})
