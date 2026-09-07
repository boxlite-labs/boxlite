import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import test from 'node:test'
import { MODULE_NAMES, run, usage } from '../src/cli/run.ts'

const helpFor = async (module: string) => {
  const lines: string[] = []
  await run({ argv: [module, '--help'], environment: {}, log: (line: string) => lines.push(line) })
  return lines.join('\n')
}

/**
 * mstage obtains and verifies access; spending it belongs to each repository's
 * own deploy tool. An example that runs one is not merely off-topic here: what
 * the CLI advertises is what a reader copies, and copying that one reaches the
 * stack past whatever gate the repository put in front of its deploys.
 *
 * Only what mstage tells a reader to run is checked. Naming SST's object layout
 * or its CLI's own rules is what keeps this store readable by the tool that
 * shares it, and those references have to stay.
 */
test('nothing the CLI advertises running is a deploy', async () => {
  // Every module the dispatcher declares, asked for rather than listed: a module
  // added later would otherwise be one this never looks at.
  assert.ok(MODULE_NAMES.length > 0, 'the dispatcher declares modules')
  const rendered = [usage(), ...(await Promise.all(MODULE_NAMES.map(helpFor)))]

  for (const text of rendered) {
    assert.ok(text.length > 0, 'the dispatcher renders help')
    for (const line of text.split('\n').filter((candidate) => candidate.includes('mstage '))) {
      assert.doesNotMatch(line, /\bsst\b/, `"${line.trim()}" runs a deploy tool`)
    }
  }
})

test('the README shows the same exec invocation the CLI advertises', async () => {
  // Two places describe one command. Checking the README against the rendered
  // help rather than against a copy of the string keeps the doc honest without
  // pinning either to prose.
  const advertised = (await helpFor('aws'))
    .split('\n')
    .map((line) => line.trim())
    .find((line) => line.includes('aws exec') && line.includes(' -- '))
  assert.ok(advertised, 'the aws module advertises how exec is called')

  const readme = readFileSync(new URL('../README.md', import.meta.url), 'utf8')
  const shown = readme
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.startsWith('npm run mstage aws exec'))
  assert.ok(shown.length > 0, 'the README shows exec too')

  const inner = (line: string) => line.slice(line.lastIndexOf(' -- ') + 4).trim()
  for (const line of shown) {
    assert.equal(inner(line), inner(advertised), 'the README runs what the CLI advertises')
  }
})

test('exec is documented as a bridge to any tool, not to a deploy', () => {
  // The reason `exec` exists is credential resolution. Writing one tool's name
  // into it is how the bridge becomes that tool's entry point.
  const handler = readFileSync(new URL('../src/cli/handlers/aws.ts', import.meta.url), 'utf8')
  const doc = handler.slice(0, handler.indexOf('import '))
  assert.match(doc, /\bexec\b/, 'the module still explains the bridge')
  assert.doesNotMatch(doc, /\bsst\b/, 'without naming a deploy tool as its purpose')
})
