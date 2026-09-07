/*
 * ClickHouse on GCP: one private instance this stack owns, or an endpoint it
 * only records.
 *
 * The AWS side of this module runs a reconcile step over SSM — SQL executed on
 * the host, because the schema and the retention are not things a cloud API can
 * express. Google has no SSM, and the equivalent is not a smaller version of
 * the same thing: the instance is reached through IAP or through a startup
 * script, and neither is a resource the engine can re-run when a password
 * rotates.
 *
 * So the self-hosted path here does the reconcile from the *startup script*
 * instead, and accepts what that costs: the schema is applied at boot rather
 * than on every deploy, and a rotated password needs a restart rather than an
 * apply. That is a real difference in behaviour and it is written down rather
 * than smoothed over — the alternative was a local command that shells out to
 * `gcloud compute ssh`, which is a person's tool wearing a resource's clothes.
 *
 * `managed` and `disabled` behave exactly as they do on AWS, because neither
 * touches a machine.
 */

import type { ClickHouse, ClickHouseProvider, ClickHouseRequest } from '../../clickhouse.ts'
import type { NetworkBinding } from '../../network.ts'

/** What each requested size answers to. */
const MACHINE = { small: 'n2-standard-2', medium: 'n2-standard-4' } as const

const HTTP_PORT = 8123

/** Ubuntu's own image family, the same release the runner hosts use. */
const IMAGE = 'ubuntu-os-cloud/ubuntu-2404-lts-amd64'

type ManagedSecret = { secret: CloudResource; version: CloudResource }

const clickHouseSecret = (resourceName: string, project: string, secretId: string): ManagedSecret => {
  const password = new random.RandomPassword(`${resourceName}Password`, { length: 32, special: false })
  const secret = new gcp.secretmanager.Secret(resourceName, {
    project,
    secretId,
    replication: { auto: {} },
  })
  const version = new gcp.secretmanager.SecretVersion(`${resourceName}Value`, {
    secret: secret.id,
    secretData: $util.secret(password.result),
  })
  return { secret, version }
}

export const gcpClickHouseProvider =
  ({
    network,
    project,
    zone,
    serviceAccount,
    managed,
    dependsOn,
  }: {
    network: Extract<NetworkBinding, { cloud: 'gcp' }>
    project: string
    /**
     * A zone in the stage's region. An instance is zonal where a subnet is not,
     * and the region itself is not needed here — the zone already names it.
     */
    zone: string
    /** The identity the host runs as, so it may read its own three secrets. */
    serviceAccount: $util.Output<string>
    managed: { url: string; writerSecretArn: string; readerSecretArn: string } | null
    dependsOn: any[]
  }): ClickHouseProvider =>
  (request: ClickHouseRequest): ClickHouse => {
    if (request.mode === 'disabled') return { active: false, mode: 'disabled' }

    if (request.mode === 'managed') {
      if (!managed) {
        throw new Error(
          'CLICKHOUSE_MODE=managed needs CLICKHOUSE_URL, CLICKHOUSE_WRITER_PASSWORD_SECRET_ARN and ' +
            'CLICKHOUSE_READER_PASSWORD_SECRET_ARN in this stage’s store',
        )
      }
      return {
        active: true,
        mode: 'managed',
        url: $util.output(managed.url),
        database: request.database,
        // Secret Manager references here rather than Secrets Manager ARNs;
        // the store holds whichever this stage's cloud uses, and the key names
        // are shared because the *idea* is. A managed endpoint's are whatever
        // was seeded, which mstage guarantees carries no version.
        writer: {
          username: request.writerUsername,
          passwordRef: $util.output(managed.writerSecretArn),
          // A pinned version name already changes when the secret rotates, so
          // there is nothing to look up: unlike an ARN, it is not stable across
          // one.
          credentialVersion: $util.output(managed.writerSecretArn),
        },
        reader: {
          username: request.readerUsername,
          passwordRef: $util.output(managed.readerSecretArn),
          credentialVersion: $util.output(managed.readerSecretArn),
        },
        binding: { cloud: 'gcp', clientGrant: serviceAccount },
        id: $util.output(managed.url),
        ready: [],
      }
    }

    const prefix = `${$app.name}-${$app.stage}`
    const admin = clickHouseSecret('ClickHouseAdminSecret', project, `${prefix}-clickhouse-admin`)
    const writer = clickHouseSecret('ClickHouseWriterSecret', project, `${prefix}-clickhouse-writer`)
    const reader = clickHouseSecret('ClickHouseReaderSecret', project, `${prefix}-clickhouse-reader`)

    const host = new gcp.serviceaccount.Account('ClickHouseServiceAccount', {
      project,
      accountId: `${prefix}-clickhouse`.slice(0, 30),
      displayName: `BoxLite ClickHouse (${$app.stage})`,
    })
    // Each secret named individually rather than a project-wide accessor role:
    // the host reads exactly its own three and nothing else in the project.
    const access = [admin, writer, reader].map(
      ({ secret }, index) =>
        new gcp.secretmanager.SecretIamMember(`ClickHouseSecretAccess${index}`, {
          project,
          secretId: secret.secretId,
          role: 'roles/secretmanager.secretAccessor',
          member: host.email.apply((email: string) => `serviceAccount:${email}`),
        }),
    )

    /*
     * The data disk, separate and retained.
     *
     * Separate because the boot disk goes with the instance whenever the
     * startup script changes; retained because a stage removed by mistake must
     * keep the history it collected. The same reasoning as the AWS side's EBS
     * volume, and the same two properties.
     */
    const disk = new gcp.compute.Disk(
      'ClickHouseData',
      { name: `${prefix}-clickhouse-data`, project, zone, size: request.dataGb, type: 'pd-balanced' },
      { retainOnDelete: true },
    )

    const startupScript = $resolve([admin.version.name, writer.version.name, reader.version.name]).apply(
      ([adminRef, writerRef, readerRef]) => `#!/bin/bash
set -euo pipefail
exec > /var/log/clickhouse-setup.log 2>&1

# The data disk, formatted once and mounted every boot.
DEVICE=/dev/disk/by-id/google-clickhouse-data
if ! blkid "$DEVICE" >/dev/null 2>&1; then mkfs.ext4 -m 0 -F "$DEVICE"; fi
mkdir -p /var/lib/clickhouse
grep -q "$DEVICE" /etc/fstab || echo "$DEVICE /var/lib/clickhouse ext4 defaults,nofail 0 2" >> /etc/fstab
mount -a

curl -fsSL https://packages.clickhouse.com/deb/pubkey.gpg | gpg --dearmor -o /usr/share/keyrings/clickhouse.gpg
echo "deb [signed-by=/usr/share/keyrings/clickhouse.gpg] https://packages.clickhouse.com/deb stable main" > /etc/apt/sources.list.d/clickhouse.list
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y clickhouse-server clickhouse-client

read_secret() { gcloud secrets versions access "$1" --format='get(payload.data)' | base64 -d; }
ADMIN=$(read_secret "${adminRef}")
WRITER=$(read_secret "${writerRef}")
READER=$(read_secret "${readerRef}")

# Listen on the private address only. The firewall says who may reach it; this
# says it will not answer anywhere else even if that changes.
cat > /etc/clickhouse-server/config.d/boxlite.xml << 'CONFIG'
<clickhouse>
  <listen_host>0.0.0.0</listen_host>
  <http_port>${HTTP_PORT}</http_port>
</clickhouse>
CONFIG

systemctl enable clickhouse-server
systemctl restart clickhouse-server
until clickhouse-client --query 'SELECT 1' >/dev/null 2>&1; do sleep 2; done

# The schema and the two accounts, applied at boot. See this file's own note:
# there is no SSM here, so this is where the reconcile lives.
clickhouse-client --query "CREATE DATABASE IF NOT EXISTS ${request.database}"
clickhouse-client --query "CREATE USER IF NOT EXISTS ${request.writerUsername} IDENTIFIED BY '$WRITER'"
clickhouse-client --query "CREATE USER IF NOT EXISTS ${request.readerUsername} IDENTIFIED BY '$READER'"
clickhouse-client --query "GRANT INSERT, SELECT, CREATE, ALTER ON ${request.database}.* TO ${request.writerUsername}"
clickhouse-client --query "GRANT SELECT ON ${request.database}.* TO ${request.readerUsername}"
clickhouse-client --query "ALTER USER default IDENTIFIED BY '$ADMIN'"
echo "clickhouse setup complete"
`,
    )

    const instance = new gcp.compute.Instance(
      'ClickHouse',
      {
        name: `${prefix}-clickhouse`,
        project,
        zone,
        machineType: MACHINE[request.instanceSize],
        bootDisk: { initializeParams: { image: IMAGE, size: 20, type: 'pd-balanced' } },
        attachedDisks: [{ source: disk.id, deviceName: 'clickhouse-data' }],
        networkInterfaces: [
          {
            subnetwork: network.subnetwork,
            // No access config, so no external address at all: the only way in
            // is from inside this network.
          },
        ],
        serviceAccount: { email: host.email, scopes: ['cloud-platform'] },
        metadataStartupScript: startupScript,
        // Telemetry storage is not a machine to be replaced casually, and a
        // newer image on an unrelated deploy would take the history with it.
        allowStoppingForUpdate: false,
      },
      { ignoreChanges: ['bootDisk'], dependsOn: [...access, ...dependsOn] },
    )

    const firewall = new gcp.compute.Firewall('ClickHouseFirewall', {
      name: `${prefix}-clickhouse`,
      project,
      network: network.network,
      direction: 'INGRESS',
      allows: [{ protocol: 'tcp', ports: [String(HTTP_PORT)] }],
      // Whoever the caller is, by identity — the collector writes and the API
      // reads, and nothing else in the network has a reason to reach this.
      sourceServiceAccounts: [serviceAccount],
      targetServiceAccounts: [host.email],
    })

    return {
      active: true,
      mode: 'self-hosted',
      url: $interpolate`http://${instance.networkInterfaces[0].networkIp}:${HTTP_PORT}`,
      database: request.database,
      writer: {
        username: request.writerUsername,
        passwordRef: writer.version.name,
        credentialVersion: writer.version.name,
      },
      reader: {
        username: request.readerUsername,
        passwordRef: reader.version.name,
        credentialVersion: reader.version.name,
      },
      binding: { cloud: 'gcp', clientGrant: serviceAccount },
      id: instance.id,
      ready: [instance, firewall],
    }
  }
