import assert from 'node:assert/strict'
import test from 'node:test'
import { listVersions, type Clients } from '../src/env/store.ts'
import { formatVersions, versions } from '../src/cli/handlers/env.ts'

const at = (iso: string) => new Date(iso)

/** Pages are returned in order; each is one ListObjectVersions answer. */
const clients = (pages: any[]) => {
  const asked: Record<string, unknown>[] = []
  let next = 0
  return {
    asked,
    clients: {
      s3: {
        send: async (command: any) => {
          asked.push(command.input)
          return pages[next++] ?? {}
        },
      },
      ssm: {
        send: async () => ({ Parameter: { Value: JSON.stringify({ state: 'sst-state-abcdefghijkl' }) } }),
      },
    } as unknown as Clients,
  }
}

const KEY = 'secret/a/dev.json'

test('versions and delete markers come back as one sequence, newest first', async () => {
  const probe = clients([
    {
      Versions: [
        { Key: KEY, VersionId: 'old', LastModified: at('2026-01-01T00:00:00Z'), Size: 10, StorageClass: 'STANDARD' },
        { Key: KEY, VersionId: 'new', LastModified: at('2026-03-01T00:00:00Z'), Size: 20, StorageClass: 'STANDARD' },
      ],
      DeleteMarkers: [{ Key: KEY, VersionId: 'tombstone', LastModified: at('2026-02-01T00:00:00Z') }],
    },
  ])
  const found = await listVersions({ clients: probe.clients, app: 'a', stage: 'dev' })
  assert.deepEqual(
    found.map((version) => [version.versionId, version.type]),
    [
      ['new', 'version'],
      ['tombstone', 'delete marker'],
      ['old', 'version'],
    ],
  )
})

test('a delete marker reports no size and no storage class, because it holds nothing', async () => {
  const probe = clients([
    { DeleteMarkers: [{ Key: KEY, VersionId: 'tombstone', LastModified: at('2026-02-01T00:00:00Z') }] },
  ])
  const [marker] = await listVersions({ clients: probe.clients, app: 'a', stage: 'dev' })
  assert.equal(marker!.size, null)
  assert.equal(marker!.storageClass, null)
})

test('a truncated listing is followed, not reported as the whole truth', async () => {
  const probe = clients([
    {
      Versions: [{ Key: KEY, VersionId: 'one', LastModified: at('2026-03-01T00:00:00Z'), Size: 1 }],
      IsTruncated: true,
      NextKeyMarker: KEY,
      NextVersionIdMarker: 'one',
    },
    { Versions: [{ Key: KEY, VersionId: 'two', LastModified: at('2026-02-01T00:00:00Z'), Size: 2 }] },
  ])
  const found = await listVersions({ clients: probe.clients, app: 'a', stage: 'dev' })
  assert.deepEqual(found.map((version) => version.versionId), ['one', 'two'])
  assert.equal(probe.asked.at(-1)?.VersionIdMarker, 'one', 'the second page continues from the first')
})

test('another object sharing the prefix is not this stage', async () => {
  // Prefix, not key: `secret/a/dev.json.bak` would come back too.
  const probe = clients([
    {
      Versions: [
        { Key: KEY, VersionId: 'mine', LastModified: at('2026-03-01T00:00:00Z'), Size: 1 },
        { Key: `${KEY}.bak`, VersionId: 'theirs', LastModified: at('2026-03-02T00:00:00Z'), Size: 1 },
      ],
    },
  ])
  const found = await listVersions({ clients: probe.clients, app: 'a', stage: 'dev' })
  assert.deepEqual(found.map((version) => version.versionId), ['mine'])
})

test('the table names the five fields, and pads nothing onto the last column', () => {
  const lines = formatVersions([
    {
      versionId: 'hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq',
      type: 'version',
      lastModified: at('2026-09-02T16:31:31Z'),
      size: 4217,
      storageClass: 'STANDARD',
    },
    { versionId: 'short', type: 'delete marker', lastModified: null, size: null, storageClass: null },
  ])
  assert.match(lines[0]!, /^VERSION ID +TYPE +LAST MODIFIED +SIZE +STORAGE CLASS$/)
  assert.ok(lines[1]!.startsWith('hb86RRZQ1eeCnZXubeB.2tzSpIIHlazq  version'))
  assert.ok(lines[1]!.endsWith('STANDARD'), 'no trailing spaces')
  assert.ok(lines[2]!.endsWith('-'), 'a delete marker prints "-" where it has nothing')
})

test('a stage with no versions says so instead of printing a bare header', async () => {
  const probe = clients([{}])
  const lines: string[] = []
  const code = await versions({
    scope: { app: 'a', stage: 'dev' } as any,
    log: (line: string) => lines.push(line),
    backend: probe.clients,
  })
  assert.equal(code, 0)
  assert.deepEqual(lines, ['# a/dev has no stored versions'])
})
