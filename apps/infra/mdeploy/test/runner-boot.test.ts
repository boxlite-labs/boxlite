/*
 * The boot script, which is the one piece of BoxLite that has to be right on a
 * machine nobody will log into.
 *
 * A runner is created once and its script is then ignored for the life of the
 * host, so a mistake here is not repaired by the next deploy. What is checked
 * is the shape a person cannot see by reading the string: that the checksum
 * gate is fatal, that the KVM check runs before anything installs, that the
 * unit's settings land somewhere systemd will read them, and that no secret is
 * written into a value the instance metadata exposes.
 */

import assert from 'node:assert/strict'
import test from 'node:test'
import { renderRunnerBoot, type BootPlatform } from '../stack/runner-boot.ts'

const platform = (overrides: Partial<BootPlatform> = {}): BootPlatform => ({
  hostAddress: 'HOST_IP=$(curl -s http://metadata/ip)',
  installVolumeMount: 'apt-get install -y some-volume-mount',
  prepareKvm: '',
  startWrapper: null,
  unitEnvironment: {},
  ...overrides,
})

const render = (overrides: Partial<Parameters<typeof renderRunnerBoot>[0]> = {}): string =>
  Buffer.from(
    renderRunnerBoot({
      apiUrl: 'https://api.dev.boxlite.ai/',
      otlpUrl: 'http://collector:4318',
      binary: { url: 'https://example.invalid/runner.tar.gz', sha256: 'c'.repeat(64) },
      port: 3003,
      environment: { BOXLITE_RUNNER_NAME: 'default' },
      platform: platform(),
      ...overrides,
    }),
    'base64',
  ).toString('utf8')

test('the script fails fast, and logs where a person can find it', () => {
  const script = render()
  assert.match(script, /^#!\/bin\/bash/)
  assert.match(script, /exec > \/var\/log\/runner-setup\.log 2>&1/)
  assert.match(script, /set -euo pipefail/)
  const fails = script.indexOf('set -euo pipefail')
  const installs = script.indexOf('apt-get update')
  assert.ok(fails < installs, 'a half-finished bootstrap must not survive its first failure')
})

test('a checksum mismatch is fatal, and is checked before the binary is installed', () => {
  const script = render()
  const verifies = script.indexOf('runner checksum mismatch')
  const installs = script.indexOf('tar -xzf')
  assert.notEqual(verifies, -1, 'the digest has to be compared, not merely fetched')
  assert.ok(verifies < installs, 'it runs as root; verifying after installing verifies nothing')
  assert.match(script, /exit 1/)
  assert.match(script, new RegExp(`"${'c'.repeat(64)}" = "\\$ACTUAL"`), 'the expected digest is the one supplied')
})

test('a host without /dev/kvm refuses to finish rather than registering', () => {
  // A box is a microVM. A host that registers and cannot start one is a silent
  // failure; one that never registers is a visible one.
  const script = render()
  const checks = script.indexOf('/dev/kvm is absent')
  const starts = script.indexOf('systemctl start boxlite-runner')
  assert.notEqual(checks, -1)
  assert.ok(checks < starts)
})

test('the KVM hook runs before the check that depends on it', () => {
  // On GCP the device exists but is not readable by the account the runner runs
  // as, and preparing it after the check would fail a host that was fine.
  const script = render({ platform: platform({ prepareKvm: 'usermod -aG kvm root' }) })
  assert.ok(script.indexOf('usermod -aG kvm root') < script.indexOf('/dev/kvm is absent'))
})

test('the unit reads its settings from a file, not from lines appended after [Install]', () => {
  // The host's own address is not known until the script runs. Appending it to
  // the unit would land it inside `[Install]`, where systemd reads it as part
  // of that section and the runner never sees it.
  const script = render()
  assert.match(script, /EnvironmentFile=\/etc\/boxlite\/runner\.env/)
  assert.match(script, /printf 'RUNNER_DOMAIN=%s\\n' "\$HOST_IP" >> \/etc\/boxlite\/runner\.env/)
  const install = script.indexOf('[Install]')
  const appends = script.indexOf("printf 'RUNNER_DOMAIN")
  assert.ok(appends > install, 'the append is outside the unit file entirely, which is the point')
  assert.doesNotMatch(script, /Environment=RUNNER_DOMAIN/, 'never as a unit line')
})

test('the API URL is normalised once, here, rather than at every caller', () => {
  const script = render()
  assert.match(script, /BOXLITE_API_URL=https:\/\/api\.dev\.boxlite\.ai\/api$/m, 'one slash, not two')
})

test('a start wrapper replaces the binary as ExecStart, and only when there is one', () => {
  const without = render()
  assert.match(without, /ExecStart=\/usr\/local\/bin\/boxlite-runner$/m)

  const wrapped = render({
    platform: platform({
      startWrapper: { path: '/usr/local/bin/boxlite-runner-start.sh', script: '# fetches secrets' },
    }),
  })
  assert.match(wrapped, /ExecStart=\/usr\/local\/bin\/boxlite-runner-start\.sh$/m)
  assert.match(wrapped, /# fetches secrets/)
})

test('no secret is written into the script, because metadata is readable on the host', () => {
  // What runs on a runner is untrusted code by design, and instance metadata is
  // readable by all of it. A secret reaches the unit through the wrapper's own
  // fetch instead.
  const script = render({
    environment: { BOXLITE_RUNNER_NAME: 'default', DEFAULT_RUNNER_API_KEY_ARN: 'arn:aws:secretsmanager:::secret:key' },
    platform: platform({
      startWrapper: {
        path: '/usr/local/bin/boxlite-runner-start.sh',
        script: 'fetch DEFAULT_RUNNER_API_KEY "$DEFAULT_RUNNER_API_KEY_ARN"',
      },
    }),
  })
  assert.match(script, /DEFAULT_RUNNER_API_KEY_ARN=arn:aws:secretsmanager:::secret:key/, 'the address may be written')
  assert.doesNotMatch(script, /^DEFAULT_RUNNER_API_KEY=/m, 'the value may not')
})

test('the platform’s own unit settings win over what the caller passed', () => {
  // A cloud names things only it can name — a region, a project — and a caller
  // that happened to set the same key must not silently retarget the host.
  const script = render({
    environment: { AWS_REGION: 'us-east-1' },
    platform: platform({ unitEnvironment: { AWS_REGION: 'ap-southeast-1' } }),
  })
  assert.match(script, /^AWS_REGION=ap-southeast-1$/m)
  assert.doesNotMatch(script, /^AWS_REGION=us-east-1$/m)
})
