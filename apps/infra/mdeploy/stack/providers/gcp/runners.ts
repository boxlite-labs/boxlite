/*
 * The runner fleet on Compute Engine, with nested virtualization.
 *
 * This is the module BoxLite exists for, and the one where GCP asks for three
 * things AWS asks for none of. Getting any of them wrong produces a host that
 * boots, registers, and fails every box with no `/dev/kvm` — so all three are
 * named here rather than assumed:
 *
 *   1. A machine family that can nest. N2, N2D and C3 can; E2 cannot, and the
 *      cheapest machine on this cloud is an E2. `MACHINE` below has no E2 in it
 *      for that reason.
 *   2. `minCpuPlatform` of Haswell or later. Google will otherwise schedule an
 *      older platform and nested virtualization is refused on it.
 *   3. `advancedMachineFeatures.enableNestedVirtualization`, explicitly.
 *
 * `scripts/deploy/gcp/create-instance.sh` has been doing exactly these three by
 * hand — `--machine-type=n2-standard-4 --min-cpu-platform='Intel Haswell'
 * --enable-nested-virtualization` — for a developer's own box host. This module
 * is that script's content, made part of a deploy, so the fleet a stage runs is
 * described in the same place as everything else it runs.
 *
 * The fourth thing that script does is `setup-kvm.sh`: on GCP the guest's
 * `/dev/kvm` is owned by `root:kvm` and the account the runner runs as is not
 * in that group, so it has to be added. That is the `prepareKvm` hook, and it
 * is the reason `stack/runner-boot.ts` has one at all.
 *
 * `deletionProtection` and `protect` are the counterparts of the AWS side's
 * `protect`: a runner holds state nothing else does, and a teardown that took
 * one with it would take every box running on it.
 */

import type { Placement } from '../../network.ts'
import type { RunnerProvider, RunnerRequest, Runners } from '../../runners.ts'
import { RUNNER_PORT } from '../../runners.ts'
import { renderRunnerBoot, type BootPlatform } from '../../runner-boot.ts'
import { splitSecretRef } from './secret-env.ts'

/**
 * What each requested size answers to.
 *
 * Every one of these is a family that can nest. E2 — which is what a
 * cost-minimising default would reach for — cannot, and a host on one accepts
 * work and fails every box.
 */
const MACHINE = { small: 'n2-standard-4', medium: 'n2-standard-8', large: 'n2-standard-16' } as const

/**
 * The oldest CPU platform that supports nested virtualization.
 *
 * Without this Google may schedule an older one, and the flag above is then
 * accepted and does nothing. The same value `scripts/deploy/gcp/
 * create-instance.sh` passes.
 */
const MIN_CPU_PLATFORM = 'Intel Haswell'

const IMAGE = 'ubuntu-os-cloud/ubuntu-2404-lts-amd64'

/**
 * The wrapper that fetches every secret this host reads, on every start.
 *
 * Fail-closed and retried, exactly as on AWS and for the same reason: at first
 * boot the service account's grants may not have propagated, and a host that
 * gave up would run without the credentials it needs rather than not run.
 */
const startWrapper = (secrets: { name: string; secret: string; version: string }[]): BootPlatform['startWrapper'] =>
  secrets.length === 0
    ? null
    : {
        path: '/usr/local/bin/boxlite-runner-start.sh',
        script: `
cat > /usr/local/bin/boxlite-runner-start.sh << 'STARTWRAP'
#!/bin/bash
# Re-fetch every secret on each start, so a rotation needs a restart rather
# than a redeploy. Fail-closed: a host that cannot read one does not start.
set -o pipefail
fetch() {
  local name="$1" secret="$2" version="$3" value=""
  for attempt in 1 2 3 4 5; do
    value=$(gcloud secrets versions access "$version" --secret="$secret" --format='get(payload.data)' 2>/dev/null | base64 -d)
    [ -n "$value" ] && break
    echo "fetch of $name attempt $attempt failed; retrying in $((attempt * 5))s" >&2
    sleep $((attempt * 5))
  done
  if [ -z "$value" ]; then
    echo "FATAL: could not read $name from $secret; refusing to start without it" >&2
    exit 1
  fi
  export "$name=$value"
}
${secrets.map(({ name, secret, version }) => `fetch ${name} ${secret} ${version}`).join('\n')}
exec /usr/local/bin/boxlite-runner
STARTWRAP
chmod +x /usr/local/bin/boxlite-runner-start.sh
`,
      }

export const gcpRunnerProvider =
  ({
    project,
    zone,
    placement,
    dependsOn,
  }: {
    project: string
    /** A zone. An instance is zonal even where its subnet is not. */
    zone: string
    placement: Extract<Placement, { cloud: 'gcp' }>
    dependsOn: any[]
  }): RunnerProvider =>
  (request: RunnerRequest): Runners => {
    if (placement.exposure !== 'egress-only-public') {
      throw new Error(
        `The runner was placed as ${placement.exposure}; it pulls box images constantly and must ` +
          'egress through its own address rather than queue behind Cloud NAT',
      )
    }

    const platform: BootPlatform = {
      // Google's metadata server. `Metadata-Flavor` is what distinguishes a
      // real request from a browser that wandered onto the address.
      hostAddress: `HOST_IP=$(curl -s -H "Metadata-Flavor: Google" http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/ip)`,
      installVolumeMount: `# gcsfuse, which is what mounts a box volume here.
curl -fsSL https://packages.cloud.google.com/apt/doc/apt-key.gpg | gpg --dearmor -o /usr/share/keyrings/cloud.google.gpg
echo "deb [signed-by=/usr/share/keyrings/cloud.google.gpg] https://packages.cloud.google.com/apt gcsfuse-noble main" > /etc/apt/sources.list.d/gcsfuse.list
apt-get update
apt-get install -y gcsfuse`,
      /*
       * What `scripts/deploy/gcp/setup-kvm.sh` does, made part of the boot.
       *
       * The device exists as soon as nested virtualization is on, but it is
       * owned by `root:kvm` and the account the runner runs as is not in that
       * group. Without this the runner starts, accepts work, and fails every
       * box on a permission error against a device that is plainly there.
       */
      prepareKvm: `# The KVM device is present but not readable by the runner's account.
# This is scripts/deploy/gcp/setup-kvm.sh, applied at boot rather than by hand.
groupadd -f kvm
usermod -aG kvm root
cat > /etc/udev/rules.d/65-kvm.rules << 'KVMRULE'
KERNEL=="kvm", GROUP="kvm", MODE="0660"
KVMRULE
udevadm control --reload-rules
udevadm trigger --name-match=kvm || true`,
      startWrapper: null,
      unitEnvironment: { CLOUDSDK_CORE_PROJECT: project },
    }

    const instances = request.fleet.map((slot) => {
      const userData = $resolve([
        request.apiUrl,
        request.otlpUrl,
        request.binary.url,
        request.binary.sha256,
        $resolve(Object.values(request.secrets)),
      ]).apply(([apiUrl, otlpUrl, url, sha256, references]) => {
        const secrets = Object.keys(request.secrets).map((name, index) => ({
          name,
          ...splitSecretRef((references as string[])[index] as string),
        }))
        return renderRunnerBoot({
          apiUrl: apiUrl as string,
          otlpUrl: otlpUrl as string,
          binary: { url: url as string, sha256: sha256 as string },
          port: RUNNER_PORT,
          environment: {
            ...(request.environment as Record<string, string>),
            BOXLITE_RUNNER_NAME: slot.controlPlaneRunnerName,
          },
          platform: { ...platform, startWrapper: startWrapper(secrets) },
        })
      })

      return new gcp.compute.Instance(
        slot.resourceName,
        {
          name: slot.nameTag,
          project,
          zone,
          machineType: MACHINE[request.size],
          // All three, and none of them optional. See the note at the top.
          minCpuPlatform: MIN_CPU_PLATFORM,
          advancedMachineFeatures: { enableNestedVirtualization: request.nestedVirtualization },
          bootDisk: {
            initializeParams: { image: IMAGE, size: request.rootDiskGb, type: 'pd-balanced' },
          },
          networkInterfaces: [
            {
              subnetwork: placement.subnetwork,
              // Its own address, so image pulls do not queue behind Cloud NAT.
              // The firewall admits only the API and the proxy, on one port.
              accessConfigs: [{}],
            },
          ],
          serviceAccount: { email: placement.serviceAccount, scopes: ['cloud-platform'] },
          // The boot script is base64 on AWS and plain text here, which is the
          // one place the two clouds want the same value differently.
          metadataStartupScript: userData.apply((encoded: string) =>
            Buffer.from(encoded, 'base64').toString('utf8'),
          ),
          labels: { 'boxlite-runner': slot.controlPlaneRunnerName.toLowerCase().replace(/[^a-z0-9_-]/g, '-') },
          // A host holds boxes. A teardown that took one with it would take
          // every box running on it.
          deletionProtection: true,
        },
        {
          // The boot script only ever runs once, and a newer image or a version
          // bump must not replace a machine with running boxes on it. Both are
          // landed on a live host out of band, one at a time.
          ignoreChanges: ['bootDisk', 'metadataStartupScript'],
          protect: true,
          dependsOn,
        },
      )
    })

    return { ids: instances.map((instance) => instance.id), ready: instances }
  }
