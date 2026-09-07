/*
 * PostgreSQL on RDS.
 *
 * The password is the interesting part. SST's `Postgres` component generates
 * one and exposes it as an output, which is how the incumbent handed it to the
 * API — as a plain value in a task definition, readable by anyone who could
 * describe that definition and copied into every revision ever registered. This
 * provider copies it into Secrets Manager once and hands the *ARN* onward, so
 * the ECS agent resolves it just before the container starts and the value
 * never enters the deploy's own outputs.
 *
 * `clientGrant` is a security group rather than a grant on the database,
 * because that is how AWS says it: the database admits a group, and a workload
 * carries membership. A workload that is not given this group cannot reach the
 * database however correct its credentials are, which is the property worth
 * having.
 *
 * The final-snapshot name is randomised. A fixed one collides with the snapshot
 * a prior teardown of the same stage already wrote, and RDS refuses a duplicate
 * — so a protected stage that was deleted anyway would fail to keep the copy
 * that protection existed to guarantee.
 */

import type { Database, DatabaseProvider, DatabaseRequest } from '../../database.ts'
import type { NetworkBinding } from '../../network.ts'

/** What each requested size answers to. Grown as a stage needs one. */
const INSTANCE = { small: 't4g.micro', medium: 'm7g.large' } as const
const STORAGE = { small: '20 GB', medium: '100 GB' } as const

export const awsDatabaseProvider =
  ({ network }: { network: Extract<NetworkBinding, { cloud: 'aws' }> }): DatabaseProvider =>
  (request: DatabaseRequest): Database => {
    const snapshotSuffix = request.protected
      ? new random.RandomId('DbFinalSnapshotSuffix', { byteLength: 4 })
      : undefined

    const postgres = new sst.aws.Postgres('Database', {
      vpc: network.vpc,
      database: request.name,
      instance: INSTANCE[request.size],
      storage: STORAGE[request.size],
      multiAz: request.highlyAvailable,
      transform: {
        instance: (args: any) => {
          args.backupRetentionPeriod = request.backupRetentionDays
          // `removal: retain` already keeps a protected stage's resources on a
          // `remove`, but it does not stop a targeted destroy, a replace on an
          // immutable change, or a delete from the console. These do.
          args.deletionProtection = request.protected
          args.skipFinalSnapshot = !request.protected
          if (snapshotSuffix) {
            args.finalSnapshotIdentifier = $interpolate`${$app.name}-${$app.stage}-db-final-${snapshotSuffix.hex}`
          }
        },
      },
    })

    /*
     * The password, moved out of the deploy's outputs.
     *
     * `$util.secret` marks it so the engine's own state seals it; the container
     * then receives the ARN rather than the value. Both halves matter — sealing
     * the state without changing the delivery would still put the password in
     * every task definition.
     */
    const secret = new aws.secretsmanager.Secret('DatabasePassword', {
      namePrefix: `${$app.name}-${$app.stage}-db-password-`,
      recoveryWindowInDays: 7,
    })
    const version = new aws.secretsmanager.SecretVersion('DatabasePasswordValue', {
      secretId: secret.id,
      secretString: $util.secret(postgres.password),
    })

    return {
      connection: {
        host: postgres.host,
        port: postgres.port.apply(String),
        username: postgres.username,
        database: postgres.database,
      },
      binding: {
        cloud: 'aws',
        // The version rather than the secret, so a rotation is a change the
        // deploy can see: the ARN alone would be identical either side of one.
        passwordRef: version.arn,
        // SST puts the database in a group that admits the VPC's own workloads,
        // which is the group every service already carries. Named here so the
        // API's provider attaches it explicitly rather than relying on the
        // default it happens to inherit.
        clientGrant: postgres.nodes.instance.vpcSecurityGroupIds.apply((groups: string[]) => groups[0] as string),
      },
      id: postgres.nodes.instance.id,
      ready: [version],
    }
  }
