import assert from 'node:assert/strict'
import test from 'node:test'
import { MODULE_NAMES, PLAN, batchName, deployBatches, type ModuleName } from '../src/plan.ts'

test('every module names a real dependency, and nothing depends on itself', () => {
  // A typo in `needs` is otherwise silent: the module simply loses an edge and
  // lands one batch too early, where the thing it needed does not exist yet.
  for (const name of MODULE_NAMES) {
    for (const need of PLAN[name].needs) {
      assert.ok(MODULE_NAMES.includes(need), `${name} needs "${need}", which is not a module`)
      assert.notEqual(need, name, `${name} cannot need itself`)
    }
  }
})

test('every module says why it waits', () => {
  // Not a style rule. A failed batch prints this, and "because the graph says
  // so" is what a person reading a 3 a.m. failure already knows.
  for (const name of MODULE_NAMES) {
    assert.ok(PLAN[name].because.length > 10, `${name} does not say why it waits`)
    assert.ok(PLAN[name].components.length > 0, `${name} names no component to target`)
  }
})

test('no two modules claim the same component', () => {
  // `--target` selects on these names, so a component in two modules would be
  // deployed by whichever batch ran first and reported by both.
  const seen = new Map<string, ModuleName>()
  for (const name of MODULE_NAMES) {
    for (const component of PLAN[name].components) {
      const owner = seen.get(component)
      assert.equal(owner, undefined, `${component} is claimed by both ${owner} and ${name}`)
      seen.set(component, name)
    }
  }
})

test('the batches are levels of the graph, so nothing lands before what it needs', () => {
  const batches = deployBatches()
  const landed = new Map<ModuleName, number>()
  batches.forEach((batch, index) => batch.forEach((name) => landed.set(name, index)))

  assert.deepEqual([...landed.keys()].sort(), [...MODULE_NAMES].sort(), 'every module is deployed exactly once')
  for (const name of MODULE_NAMES) {
    for (const need of PLAN[name].needs) {
      assert.ok(
        (landed.get(need) as number) < (landed.get(name) as number),
        `${name} lands in batch ${landed.get(name)} but ${need} lands in ${landed.get(need)}`,
      )
    }
  }
})

test('two modules share a batch exactly when neither can reach the other', () => {
  // The grouping is derived rather than chosen, which is what makes adding a
  // dependency split a batch by itself. Reachability, not just direct needs: a
  // module that reaches another through a third must not run beside it.
  const reaches = (from: ModuleName, to: ModuleName, seen = new Set<ModuleName>()): boolean => {
    if (seen.has(from)) return false
    seen.add(from)
    return PLAN[from].needs.some((need) => need === to || reaches(need, to, seen))
  }
  for (const batch of deployBatches()) {
    for (const one of batch) {
      for (const other of batch) {
        if (one === other) continue
        assert.ok(!reaches(one, other), `${one} reaches ${other} and cannot share its batch`)
      }
    }
  }
})

test('the network is first and the alarms are last', () => {
  // Both are deliberate and both would be easy to lose. Nothing can be placed
  // before there is somewhere to place it; and an alarm that cannot be created
  // must not roll back a service that is already serving.
  const batches = deployBatches()
  assert.ok(batches[0]!.includes('network'), 'the network has to be in the first batch')
  assert.deepEqual(batches.at(-1), ['alarms'], 'alarms come last, alone')
})

test('a batch names itself after what it deploys', () => {
  assert.equal(batchName(['network', 'storage']), 'network-storage')
  assert.equal(batchName(['alarms']), 'alarms')
})
