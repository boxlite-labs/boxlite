/*
 * What a runner host does at first boot, written once for both clouds.
 *
 * The script is the same shape everywhere, because what it does is the same
 * everywhere: install what a box needs, fetch the runner binary, refuse to
 * install it unless the checksum matches, and hand it to systemd. Four things
 * genuinely differ, and they are the four hooks below — where the host learns
 * its own address, how a private registry credential is fetched, what mounts a
 * volume, and what has to be done in the guest to reach `/dev/kvm`.
 *
 * The last one is the reason this file is not simply AWS's script with a couple
 * of substitutions. On AWS nested virtualization is an instance attribute and
 * the guest needs nothing; on GCP it also needs the guest's KVM device to be
 * readable by the account the runner runs as, which is what
 * `scripts/deploy/gcp/setup-kvm.sh` has been doing by hand. A shared renderer
 * with an explicit hook is what stops that from being a step someone remembers.
 *
 * No secret is ever written into this script. The registration token is the one
 * value that looks like an exception and is not: it reaches the unit file
 * through a fetch the hook renders, for the same reason the registry credential
 * does — a value baked in here would be readable from the instance metadata by
 * anything running on the host, which on a runner is untrusted code by design.
 */

/** What one cloud contributes to the boot script. */
export type BootPlatform = {
  /** Shell that sets `HOST_IP` to the address the control plane reaches. */
  hostAddress: string
  /**
   * Shell that installs whatever mounts a volume. Different products, same job:
   * Mountpoint for Amazon S3 on one cloud, gcsfuse on the other.
   */
  installVolumeMount: string
  /**
   * Shell that makes `/dev/kvm` usable. Empty where the platform already has.
   * See the note above: this is the hook that exists for GCP's sake.
   */
  prepareKvm: string
  /**
   * A start wrapper that re-fetches every secret the unit needs, or null where
   * the host has none to fetch.
   *
   * A wrapper rather than a one-time fetch so a `systemctl restart` picks up a
   * rotated credential, and fail-closed so a host that cannot read one refuses
   * to start rather than falling back to anonymous pulls.
   */
  startWrapper: { script: string; path: string } | null
  /** `Environment=` lines the unit needs and only this cloud can name. */
  unitEnvironment: Record<string, string>
}

export type BootInput = {
  /** Where the host registers itself. The `/api` prefix is added here. */
  apiUrl: string
  /** Where it ships telemetry. */
  otlpUrl: string
  /** What to install, and the digest that has to match before it is. */
  binary: { url: string; sha256: string }
  /** The port the runner multiplexes everything onto. */
  port: number
  /** Values the unit carries beyond what this file decides. */
  environment: Record<string, string>
  platform: BootPlatform
}

/**
 * The boot script, base64-encoded the way both clouds want it.
 *
 * `set -euo pipefail` and the log redirect are first and are not decoration: a
 * half-finished bootstrap must not leave a host that looks up and has silently
 * skipped the checksum verification.
 */
export const renderRunnerBoot = (input: BootInput): string => {
  const { platform } = input
  const execStart = platform.startWrapper ? platform.startWrapper.path : '/usr/local/bin/boxlite-runner'
  const unitEnvironment = {
    BOXLITE_API_URL: `${input.apiUrl.replace(/\/$/, '')}/api`,
    API_VERSION: '2',
    API_PORT: String(input.port),
    BOXLITE_HOME_DIR: '/var/lib/boxlite',
    OTEL_LOGGING_ENABLED: 'true',
    OTEL_TRACING_ENABLED: 'true',
    OTEL_EXPORTER_OTLP_ENDPOINT: input.otlpUrl,
    ...input.environment,
    ...platform.unitEnvironment,
  }

  const script = `#!/bin/bash
exec > /var/log/runner-setup.log 2>&1
# Fail fast and loud: a half-finished bootstrap must not leave a runner that
# looks up but silently skipped the binary download or its checksum check.
set -euo pipefail

# A package manager still holding the lock from the image's own first boot.
while fuser /var/lib/dpkg/lock-frontend >/dev/null 2>&1; do sleep 5; done

apt-get update
apt-get install -y curl ca-certificates

${platform.installVolumeMount}

${platform.prepareKvm}
# A box is a microVM, so a host without /dev/kvm can accept work and fail every
# single request. Refuse to finish the bootstrap instead — an instance that
# never registers is a visible failure; one that registers and cannot start a
# box is a silent one.
[ -e /dev/kvm ] || { echo "FATAL: /dev/kvm is absent; this host cannot run boxes" >&2; exit 1; }

# Fetch the binary and verify it before installing: it runs as root. A mismatch
# is fatal rather than a warning.
curl -fsSL "${input.binary.url}" -o /tmp/boxlite-runner.tar.gz
ACTUAL=$(sha256sum /tmp/boxlite-runner.tar.gz | awk '{print $1}')
[ "${input.binary.sha256}" = "$ACTUAL" ] || {
  echo "FATAL: runner checksum mismatch (want ${input.binary.sha256} got $ACTUAL)" >&2
  exit 1
}
echo "runner tarball checksum verified ($ACTUAL)"
tar -xzf /tmp/boxlite-runner.tar.gz -C /usr/local/bin/
rm -f /tmp/boxlite-runner.tar.gz
chmod +x /usr/local/bin/boxlite-runner

${platform.hostAddress}
${platform.startWrapper?.script ?? ''}
# The unit's settings live in a file rather than in Environment= lines.
#
# One of them — the host's own address — is not known until this script runs,
# and appending it to the unit would land it after [Install], where systemd
# reads it as part of that section and the runner never sees it. A file is also
# what makes a rotated value a rewrite and a restart rather than a redeploy.
mkdir -p -m 750 /etc/boxlite
cat > /etc/boxlite/runner.env << 'RUNNERENV'
${Object.entries(unitEnvironment)
  .map(([key, value]) => `${key}=${value}`)
  .join('\n')}
RUNNERENV
printf 'RUNNER_DOMAIN=%s\\n' "$HOST_IP" >> /etc/boxlite/runner.env
chmod 640 /etc/boxlite/runner.env

cat > /etc/systemd/system/boxlite-runner.service << 'UNIT'
[Unit]
Description=BoxLite Runner
After=network.target

[Service]
Type=simple
ExecStart=${execStart}
EnvironmentFile=/etc/boxlite/runner.env
Restart=always
RestartSec=5
# The runner budgets 30s internally to stop its VMs; 60s leaves headroom for
# in-flight handlers and the deferred close.
TimeoutStopSec=60

[Install]
WantedBy=multi-user.target
UNIT

mkdir -p /var/lib/boxlite
systemctl daemon-reload
systemctl enable boxlite-runner
systemctl start boxlite-runner

echo "Runner setup complete"
`
  return Buffer.from(script).toString('base64')
}
