/*
 * The runner fleet on EC2.
 *
 * Nested virtualization is one instance attribute here — `cpuOptions.
 * nestedVirtualization` — and the guest needs nothing, which is why this
 * provider's `prepareKvm` hook is empty and GCP's is not.
 *
 * Two Pulumi options keep a host alive across routine deploys, and both are
 * load-bearing rather than cautious. A runner holds state no other resource
 * does — `/var/lib/boxlite` and the libkrun VMs in its memory — so
 * `ignoreChanges` on the image and the boot script stops a monthly Ubuntu
 * release or a version bump from replacing a machine with running boxes on it,
 * and `protect` refuses a delete outright. The version bump still has to reach
 * the fleet; it does so over SSM, one host at a time, outside a deploy.
 *
 * A consequence worth stating: because the boot script is ignored after the
 * first boot, its dependencies have to be right the *first* time. The artifact
 * grant is a sibling of the instance rather than an ancestor, so without an
 * explicit edge Pulumi may create the host first and its boot script dies on
 * AccessDenied — permanently, because it never runs again.
 *
 * Secrets reach a host through a start wrapper that re-fetches them on every
 * start, not through the boot script. Anything written into user data is
 * readable from the instance metadata by whatever runs on the host, and what
 * runs on a runner is untrusted code by design.
 */

import type { Placement } from '../../network.ts'
import type { RunnerProvider, RunnerRequest, Runners } from '../../runners.ts'
import { RUNNER_PORT } from '../../runners.ts'
import { renderRunnerBoot, type BootPlatform } from '../../runner-boot.ts'

/** What each requested size answers to. Every one of these can nest. */
const INSTANCE = { small: 'c8i.large', medium: 'c8i.xlarge', large: 'c8i.2xlarge' } as const

const UBUNTU_OWNER = '099720109477'
const UBUNTU_NAME = 'ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*'

/** Mountpoint for Amazon S3, which is what mounts a volume on this cloud. */
const MOUNT_S3_VERSION = '1.20.0'

/**
 * The wrapper that fetches every secret this host reads, on every start.
 *
 * Fail-closed and retried: at first boot the instance profile's grants may not
 * have propagated yet, and a host that gave up would run without the
 * credentials it needs rather than without starting. Five attempts with a
 * growing pause, then a refusal.
 */
const startWrapper = (names: string[], region: string): BootPlatform['startWrapper'] =>
  names.length === 0
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
  local name="$1" arn="$2" value=""
  for attempt in 1 2 3 4 5; do
    value=$(aws secretsmanager get-secret-value --region "${region}" --secret-id "$arn" --query SecretString --output text 2>/dev/null)
    { [ -n "$value" ] && [ "$value" != "None" ]; } && break
    echo "fetch of $name attempt $attempt failed; retrying in $((attempt * 5))s" >&2
    sleep $((attempt * 5))
  done
  if [ -z "$value" ] || [ "$value" = "None" ]; then
    echo "FATAL: could not read $name from $arn; refusing to start without it" >&2
    exit 1
  fi
  export "$name=$value"
}
${names.map((name) => `fetch ${name} "$${name}_ARN"`).join('\n')}
exec /usr/local/bin/boxlite-runner
STARTWRAP
chmod +x /usr/local/bin/boxlite-runner-start.sh
`,
      }

export const awsRunnerProvider =
  ({
    placement,
    region,
    artifactsBucket,
    dependsOn,
  }: {
    placement: Extract<Placement, { cloud: 'aws' }>
    region: string
    /** Where a build-mode binary is staged. Read-only, and only under `runner/`. */
    artifactsBucket: string
    dependsOn: any[]
  }): RunnerProvider =>
  (request: RunnerRequest): Runners => {
    if (placement.exposure !== 'egress-only-public') {
      throw new Error(
        `The runner was placed as ${placement.exposure}; it pulls box images constantly and must ` +
          'egress through the internet gateway rather than the NAT the services share',
      )
    }

    const role = new aws.iam.Role('RunnerRole', {
      assumeRolePolicy: JSON.stringify({
        Version: '2012-10-17',
        Statement: [{ Effect: 'Allow', Principal: { Service: 'ec2.amazonaws.com' }, Action: 'sts:AssumeRole' }],
      }),
    })
    // How a binary upgrade reaches a live host, and the only way in: there is
    // no inbound port for a person.
    new aws.iam.RolePolicyAttachment('RunnerSsmPolicy', {
      role: role.name,
      policyArn: 'arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore',
    })
    /*
     * Exactly Mountpoint for Amazon S3's documented permission set, against the
     * volume buckets alone. Bucket lifecycle — create, tag, delete — is the
     * API's, not a runner's: a compromised runner must not be able to delete
     * the volume it is serving.
     */
    new aws.iam.RolePolicy('RunnerVolumeS3Policy', {
      role: role.name,
      policy: JSON.stringify({
        Version: '2012-10-17',
        Statement: [
          { Effect: 'Allow', Action: ['s3:ListBucket'], Resource: ['arn:aws:s3:::boxlite-volume-*'] },
          {
            Effect: 'Allow',
            Action: ['s3:GetObject', 's3:PutObject', 's3:DeleteObject', 's3:AbortMultipartUpload'],
            Resource: ['arn:aws:s3:::boxlite-volume-*/*'],
          },
        ],
      }),
    })
    // Read-only, and only under the prefix a build-mode binary is staged in.
    // Nothing else in the bucket is reachable, and a runner can never write here.
    const artifactPolicy = new aws.iam.RolePolicy('RunnerArtifactS3Policy', {
      role: role.name,
      policy: JSON.stringify({
        Version: '2012-10-17',
        Statement: [{ Effect: 'Allow', Action: ['s3:GetObject'], Resource: `arn:aws:s3:::${artifactsBucket}/runner/*` }],
      }),
    })

    const secretNames = Object.keys(request.secrets)
    const secretPolicy =
      secretNames.length > 0
        ? new aws.iam.RolePolicy('RunnerSecretPolicy', {
            role: role.name,
            policy: $resolve(Object.values(request.secrets)).apply((arns: string[]) =>
              JSON.stringify({
                Version: '2012-10-17',
                Statement: [{ Effect: 'Allow', Action: ['secretsmanager:GetSecretValue'], Resource: arns }],
              }),
            ),
          })
        : undefined

    const profile = new aws.iam.InstanceProfile('RunnerProfile', { role: role.name })

    const ami = aws.ec2.getAmiOutput({
      mostRecent: true,
      owners: [UBUNTU_OWNER],
      filters: [
        { name: 'name', values: [UBUNTU_NAME] },
        { name: 'architecture', values: ['x86_64'] },
      ],
    })

    const platform: BootPlatform = {
      // IMDSv2, which the instance below requires. A v1 read would fail here
      // rather than silently returning nothing.
      hostAddress: `IMDS_TOKEN=$(curl -sX PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 300")
HOST_IP=$(curl -s -H "X-aws-ec2-metadata-token: $IMDS_TOKEN" http://169.254.169.254/latest/meta-data/local-ipv4)`,
      installVolumeMount: `# Mountpoint for Amazon S3, which is what mounts a box volume here.
curl -fsSL "https://s3.amazonaws.com/mountpoint-s3-release/${MOUNT_S3_VERSION}/x86_64/mount-s3-${MOUNT_S3_VERSION}-x86_64.deb" -o /tmp/mount-s3.deb
apt-get install -y /tmp/mount-s3.deb
rm -f /tmp/mount-s3.deb

# The AWS CLI, unconditionally rather than only where a path here uses it: a
# host created against a published release can later be upgraded to a binary
# staged in S3, and that upgrade runs over SSM with no chance to install first.
curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o /tmp/awscliv2.zip
apt-get install -y unzip
unzip -q /tmp/awscliv2.zip -d /tmp
/tmp/aws/install --update
rm -rf /tmp/awscliv2.zip /tmp/aws`,
      // Nothing: nested virtualization is an instance attribute on this cloud,
      // and the guest's own /dev/kvm is present as soon as it is set.
      prepareKvm: '',
      startWrapper: startWrapper(secretNames, region),
      unitEnvironment: { AWS_REGION: region },
    }

    const instances = request.fleet.map(
      (slot) =>
        new aws.ec2.Instance(
          slot.resourceName,
          {
            ami: ami.id,
            instanceType: INSTANCE[request.size],
            // Public subnet, and the security group the network arranged is
            // what makes the address egress-only.
            subnetId: placement.subnets[0],
            associatePublicIpAddress: true,
            vpcSecurityGroupIds: placement.securityGroups,
            iamInstanceProfile: profile.name,
            cpuOptions: { nestedVirtualization: request.nestedVirtualization ? 'enabled' : 'disabled' },
            // IMDSv2 and one hop, so a container escape or an SSRF on this
            // untrusted-code host cannot read the instance role's credentials.
            metadataOptions: { httpEndpoint: 'enabled', httpTokens: 'required', httpPutResponseHopLimit: 1 },
            userDataBase64: $resolve([request.apiUrl, request.otlpUrl, request.binary.url, request.binary.sha256]).apply(
              ([apiUrl, otlpUrl, url, sha256]) =>
                renderRunnerBoot({
                  apiUrl,
                  otlpUrl,
                  binary: { url, sha256 },
                  port: RUNNER_PORT,
                  environment: {
                    ...(request.environment as Record<string, string>),
                    // Which host this is, as the control plane knows it.
                    BOXLITE_RUNNER_NAME: slot.controlPlaneRunnerName,
                    // The wrapper reads each secret's address from here.
                    ...Object.fromEntries(secretNames.map((name) => [`${name}_ARN`, String(request.secrets[name])])),
                  },
                  platform,
                }),
            ),
            rootBlockDevice: { volumeSize: request.rootDiskGb, encrypted: true },
            tags: { Name: slot.nameTag, 'boxlite:control-plane-runner-name': slot.controlPlaneRunnerName },
          },
          {
            // See the note at the top: a host holds state nothing else does.
            ignoreChanges: ['ami', 'userDataBase64'],
            protect: true,
            // The boot script reads the staged artifact with this role, and it
            // only ever runs once. Without the edge the host may be created
            // first and fail permanently.
            dependsOn: [artifactPolicy, ...(secretPolicy ? [secretPolicy] : []), ...dependsOn],
          },
        ),
    )

    return { ids: instances.map((instance) => instance.id), ready: instances }
  }
