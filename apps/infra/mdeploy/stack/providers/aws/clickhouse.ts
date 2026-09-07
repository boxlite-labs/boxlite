/*
 * ClickHouse on AWS: one private instance this stack owns, or an endpoint it
 * only records.
 *
 * The self-hosted path is a single EC2 instance with a separate, retained EBS
 * volume for its data. Separate on purpose — the instance is replaced whenever
 * its user data changes, and telemetry that lived on the root disk would go
 * with it. `retainOnDelete` on the volume is the other half: a stage removed by
 * mistake keeps the history it collected.
 *
 * Three accounts rather than two. The admin account is the one the host
 * configures itself with and no workload ever holds; the writer and the reader
 * are what the collector and the API are given. Three because the reconcile
 * step below has to be able to create and re-grant the other two, which a
 * writer cannot do.
 *
 * The schema and the retention are reconciled by a local command rather than by
 * a resource, because neither is something the AWS API can express: they are
 * SQL, executed on the instance through SSM. It is triggered on everything that
 * would change the answer — the instance, the volume, the user data, and each
 * secret's version — so a rotated password re-grants rather than locking the
 * collector out until someone notices.
 */

import {
  CLICKHOUSE_IMAGE,
  CLICKHOUSE_RETENTION_HOURS,
  encodeClickHouseUserData,
  renderClickHouseSchema,
} from '../../../../scripts/clickhouse-host.js'
import type { ClickHouse, ClickHouseProvider, ClickHouseRequest } from '../../clickhouse.ts'
import type { NetworkBinding } from '../../network.ts'

/** What each requested size answers to. */
const INSTANCE = { small: 'm6a.large', medium: 'm6a.xlarge' } as const

/** The HTTP interface. The only port anything speaks to it on. */
const HTTP_PORT = 8123

/** Ubuntu's own publisher, and the image line the runner uses too. */
const UBUNTU_OWNER = '099720109477'
const UBUNTU_NAME = 'ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*'

type ManagedSecret = { resource: CloudResource; version: CloudResource }

const clickHouseSecret = (resourceName: string, name: string): ManagedSecret => {
  const password = new random.RandomPassword(`${resourceName}Password`, { length: 32, special: false })
  const resource = new aws.secretsmanager.Secret(resourceName, {
    namePrefix: `${$app.name}-${$app.stage}-${name}-`,
    recoveryWindowInDays: 7,
  })
  const version = new aws.secretsmanager.SecretVersion(`${resourceName}Value`, {
    secretId: resource.id,
    secretString: $util.secret(password.result),
  })
  return { resource, version }
}

/**
 * The version currently marked AWSCURRENT for a secret this stack did not
 * create.
 *
 * A managed stage names two secrets by ARN and nothing else; the version is
 * what makes a rotation visible to a running container, so it is read back
 * rather than assumed. Exactly one has to be current — more than one means a
 * rotation is mid-flight, and picking either would deploy a credential that is
 * about to stop working.
 */
const currentVersion = (name: string, secretId: string, region: string): $util.Output<string> =>
  aws.secretsmanager.getSecretVersionsOutput({ secretId, region }).versions.apply((versions: any[]) => {
    const current = versions.filter((version) => version.versionStages.includes('AWSCURRENT'))
    if (current.length !== 1) throw new Error(`${name} must have exactly one AWSCURRENT version`)
    return current[0].versionId as string
  })

export const awsClickHouseProvider =
  ({
    network,
    region,
    managed,
  }: {
    network: Extract<NetworkBinding, { cloud: 'aws' }>
    region: string
    /** The endpoint and its two secrets, for a stage that runs none of its own. */
    managed: { url: string; writerSecretArn: string; readerSecretArn: string } | null
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
        writer: {
          username: request.writerUsername,
          passwordRef: $util.output(managed.writerSecretArn),
          credentialVersion: currentVersion('CLICKHOUSE_WRITER_PASSWORD_SECRET_ARN', managed.writerSecretArn, region),
        },
        reader: {
          username: request.readerUsername,
          passwordRef: $util.output(managed.readerSecretArn),
          credentialVersion: currentVersion('CLICKHOUSE_READER_PASSWORD_SECRET_ARN', managed.readerSecretArn, region),
        },
        // A managed endpoint admits whoever holds its credential; there is no
        // group to carry. The empty string is the honest answer, and the API's
        // provider skips a grant it cannot attach.
        binding: { cloud: 'aws', clientGrant: $util.output('') },
        id: $util.output(managed.url),
        ready: [],
      }
    }

    const admin = clickHouseSecret('ClickHouseAdminSecret', 'clickhouse-admin')
    const writer = clickHouseSecret('ClickHouseWriterSecret', 'clickhouse-writer')
    const reader = clickHouseSecret('ClickHouseReaderSecret', 'clickhouse-reader')

    const role = new aws.iam.Role('ClickHouseRole', {
      name: `${$app.name}-${$app.stage}-clickhouse-instance`,
      assumeRolePolicy: JSON.stringify({
        Version: '2012-10-17',
        Statement: [{ Effect: 'Allow', Principal: { Service: 'ec2.amazonaws.com' }, Action: 'sts:AssumeRole' }],
      }),
    })
    // The reconcile step below reaches the host through SSM rather than SSH, so
    // there is no key to distribute and no inbound port to open for it.
    new aws.iam.RolePolicyAttachment('ClickHouseSsmPolicy', {
      role: role.name,
      policyArn: 'arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore',
    })
    const secretPolicy = new aws.iam.RolePolicy('ClickHouseSecretPolicy', {
      role: role.name,
      policy: $resolve([admin.resource.arn, writer.resource.arn, reader.resource.arn]).apply(([...arns]) =>
        JSON.stringify({
          Version: '2012-10-17',
          Statement: [{ Effect: 'Allow', Action: ['secretsmanager:GetSecretValue'], Resource: arns }],
        }),
      ),
    })
    const profile = new aws.iam.InstanceProfile('ClickHouseProfile', {
      name: `${$app.name}-${$app.stage}-clickhouse-instance`,
      role: role.name,
    })

    const securityGroup = new aws.ec2.SecurityGroup('ClickHouseSecurityGroup', {
      vpcId: network.vpcId,
      description: 'Private ClickHouse HTTP access from BoxLite services',
      ingress: [{ protocol: 'tcp', fromPort: HTTP_PORT, toPort: HTTP_PORT, securityGroups: network.vpc.securityGroups }],
      egress: [{ protocol: '-1', fromPort: 0, toPort: 0, cidrBlocks: ['0.0.0.0/0'] }],
    })

    const subnet = aws.ec2.getSubnetOutput({ id: network.privateSubnets[0] })
    const volume = new aws.ebs.Volume(
      'ClickHouseData',
      {
        availabilityZone: subnet.availabilityZone,
        size: request.dataGb,
        type: 'gp3',
        encrypted: true,
        tags: { Name: `${$app.name}-${$app.stage}-clickhouse-data` },
      },
      // Retained on purpose: the instance is replaced whenever its user data
      // changes, and the history must not go with it.
      { retainOnDelete: true },
    )

    const ami = aws.ec2.getAmiOutput({
      mostRecent: true,
      owners: [UBUNTU_OWNER],
      filters: [
        { name: 'name', values: [UBUNTU_NAME] },
        { name: 'architecture', values: ['x86_64'] },
      ],
    })

    const userData = $resolve([volume.id, admin.resource.arn, writer.resource.arn, reader.resource.arn]).apply(
      ([volumeId, adminSecretArn, writerSecretArn, readerSecretArn]) =>
        encodeClickHouseUserData({ region, volumeId, adminSecretArn, writerSecretArn, readerSecretArn }),
    )
    const instance = new aws.ec2.Instance(
      'ClickHouse',
      {
        ami: ami.id,
        instanceType: INSTANCE[request.instanceSize],
        subnetId: network.privateSubnets[0],
        associatePublicIpAddress: false,
        vpcSecurityGroupIds: [securityGroup.id],
        iamInstanceProfile: profile.name,
        metadataOptions: { httpEndpoint: 'enabled', httpTokens: 'required', httpPutResponseHopLimit: 1 },
        userDataBase64: userData,
        userDataReplaceOnChange: true,
        rootBlockDevice: { encrypted: true, volumeType: 'gp3', volumeSize: 20 },
        tags: { Name: `${$app.name}-${$app.stage}-clickhouse` },
      },
      {
        // A newer Ubuntu image must not replace a running telemetry host on
        // an unrelated deploy; upgrading is a deliberate act.
        ignoreChanges: ['ami'],
        deleteBeforeReplace: true,
        dependsOn: [secretPolicy, admin.version, writer.version, reader.version],
      },
    )
    const attachment = new aws.ec2.VolumeAttachment(
      'ClickHouseDataAttachment',
      { deviceName: '/dev/sdf', instanceId: instance.id, volumeId: volume.id, stopInstanceBeforeDetaching: true },
      { deleteBeforeReplace: true },
    )
    const ready = new command.local.Command(
      'ClickHouseDatabaseReady',
      {
        dir: $cli.paths.root,
        create: 'node apps/infra/scripts/clickhouse-ops.mjs reconcile',
        update: 'node apps/infra/scripts/clickhouse-ops.mjs reconcile',
        environment: {
          AWS_REGION: region,
          CLICKHOUSE_INSTANCE_ID: instance.id,
          CLICKHOUSE_ADMIN_SECRET_ARN: admin.resource.arn,
          CLICKHOUSE_WRITER_SECRET_ARN: writer.resource.arn,
          CLICKHOUSE_READER_SECRET_ARN: reader.resource.arn,
          CLICKHOUSE_EXPECTED_IMAGE: CLICKHOUSE_IMAGE,
          CLICKHOUSE_SCHEMA_BASE64: Buffer.from(renderClickHouseSchema()).toString('base64'),
          CLICKHOUSE_RETENTION_HOURS: String(CLICKHOUSE_RETENTION_HOURS),
        },
        // Every input that changes the answer. A rotated password re-grants
        // rather than locking the collector out until someone notices.
        triggers: [
          instance.id,
          volume.id,
          userData,
          admin.version.id,
          writer.version.id,
          reader.version.id,
        ],
      },
      { dependsOn: [attachment] },
    )

    return {
      active: true as const,
      mode: 'self-hosted' as const,
      url: $interpolate`http://${instance.privateIp}:${HTTP_PORT}`,
      database: request.database,
      writer: {
        username: request.writerUsername,
        passwordRef: writer.resource.arn,
        credentialVersion: writer.version.versionId,
      },
      reader: {
        username: request.readerUsername,
        passwordRef: reader.resource.arn,
        credentialVersion: reader.version.versionId,
      },
      binding: { cloud: 'aws' as const, clientGrant: securityGroup.id },
      id: instance.id,
      ready: [ready],
    }
  }
