/*
 * mdeploy's own stack.
 *
 * SST is still the engine on AWS — the providers are written against its
 * components, and nothing else can instantiate an `sst.aws.Service`. What
 * changed is which file describes the stack: this one composes the modules, and
 * the one beside `mstage.config.json` is the incumbent it replaces.
 *
 * The app name and every module's logical resource names are deliberately
 * identical to the incumbent's. State in SST is keyed by app and stage, so this
 * file adopts the resources the incumbent created rather than building a second
 * copy of them — which is what makes the cutover a diff to read rather than a
 * migration to perform. Read that diff before running it:
 * `npm run mdeploy -- --stage <stage> --diff`.
 *
 * Everything a single deploy decides arrives in the environment, exactly as the
 * incumbent takes it. mdeploy puts the stage's own configuration there before
 * calling this, so a value declared in the store is read from the store and not
 * from a second place.
 */

/** Configuration that must be supplied, with the reason it matters. */
const required = (key: string, why: string): string => {
  const value = process.env[key]?.trim()
  if (!value) throw new Error(`${key} is required — ${why}`)
  return value
}

/*
 * The region this stack builds in, resolved by mstage and injected by mdeploy.
 * Read rather than written, because `mstage.config.json` declares a stage's
 * region and a literal here would be a second declaration — the one that keeps
 * building in the old region on the day a stage moves, silently, because SST
 * defaults a missing region instead of failing.
 *
 * No fallback: mdeploy is the only way in, and it always sets this.
 */
const AWS_REGION = required('AWS_REGION', 'mdeploy injects the region mstage resolved for this stage')

/** The one stage that keeps its resources when the stack is removed. */
const PRODUCTION_STAGE = 'prod'

export default $config({
  async app(input) {
    return {
      name: 'boxlite',
      removal: input?.stage === PRODUCTION_STAGE ? 'retain' : 'remove',
      protect: input?.stage === PRODUCTION_STAGE,
      home: 'aws',
      providers: {
        aws: {
          version: '7.24.0',
          region: AWS_REGION,
          ...(process.env.AWS_PROFILE ? { profile: process.env.AWS_PROFILE } : {}),
        },
        // Every public record this stack writes, on either cloud. Cloudflare is
        // the DNS in both, which is the one part of the front door that did not
        // have to be replaced when a second cloud arrived.
        cloudflare: '6.15.0',
        random: '4.16.6',
        // ClickHouse's schema and retention are SQL executed over SSM, which is
        // not something the AWS API can express as a resource.
        command: '1.0.1',
      },
    }
  },

  async run() {
    /*
     * Every role this deploy creates carries the account's runtime boundary.
     *
     * Set here rather than in each module because it applies to roles the
     * modules never name — the ones SST creates for a service or a task on
     * their behalf. Without it a task can reach another stage's secrets, which
     * is exactly what the boundary exists to refuse.
     */
    const boundary = $interpolate`arn:aws:iam::${aws.getCallerIdentityOutput({}).accountId}:policy/${$app.name}-${$app.stage}-runtime-boundary`
    $transform(aws.iam.Role, (args: any) => {
      args.permissionsBoundary ??= boundary
    })

    /*
     * Imported here, not at the top of the file: sst refuses a config with top
     * level imports, because it evaluates `app()` before the platform and its
     * providers exist.
     */
    const { loadDeployConfig } = await import('./src/config.ts')
    const { readStackEnvironment } = await import('./src/stack-env.ts')
    const { deployStack } = await import('./stack/index.ts')
    const { loadConfig: loadStageConfig } = await import('mstage/config')
    const { awsStackProviders } = await import('./stack/providers/aws/index.ts')

    const config = loadDeployConfig()
    const stageConfig = loadStageConfig()

    /*
     * What this one deploy decides, read and checked in one place.
     *
     * Shared with the Pulumi engine (`mdeploy/pulumi/program.ts`) rather than
     * re-read here. Turning a store into a workload's own channels is policy —
     * which keys are required, which pair with each other, which travel as an
     * address rather than a value — and a second copy that drifted would deploy
     * a control plane behaving differently on one cloud.
     */
    const stackEnvironment = readStackEnvironment({
      environment: process.env,
      // The one declaration of which keys are a service's, not the deploy's.
      declaration: {
        groups: stageConfig.envSelectGroup,
        optional: stageConfig.envOptional,
        where: stageConfig.path,
      },
      stage: $app.stage,
      region: AWS_REGION,
      home: 'aws',
    })

    const accountId = await aws.getCallerIdentity({}).then(({ accountId: id }) => id as string)

    return deployStack({
      /*
       * The AWS bundle, and only ever that one.
       *
       * This file is the AWS engine's own config: a GCP stage never reaches it,
       * because `src/deploy.ts` resolves the cloud first and answers a GCP one
       * with the Pulumi target instead. A second GCP branch here would be a
       * second GCP deploy path — one that could not work anyway, since the
       * region above is AWS's.
       */
      providers: awsStackProviders({
        stage: $app.stage,
        region: AWS_REGION,
        accountId,
        domain: stackEnvironment.domain,
        // The stage bootstrap owns this bucket for the same ordering reason it
        // owns the image repository: CI stages the object before this stack can
        // consume one, so the consumer cannot also create its own input.
        artifactsBucket: `${$app.name}-app-${$app.stage}-artifacts-${accountId}`,
        managedClickHouse: stackEnvironment.managedClickHouse,
      }),
      config,
      inputs: {
        stage: $app.stage,
        tag: stackEnvironment.tag,
        domain: stackEnvironment.domain,
        proxyDomain: stackEnvironment.proxyDomain,
        proxyProtocol: stackEnvironment.proxyProtocol,
        // Outbound HTTPS for the private workloads, through the NAT. Inbound is
        // the load balancers' alone; this is the other direction, which every
        // image pull needs.
        internetEgress: true,
        senderDomain: stackEnvironment.senderDomain,
        runnerBinary: stackEnvironment.runnerBinary,
        runnerFleet: stackEnvironment.runnerFleet,
        apiEnvironment: stackEnvironment.apiEnvironment,
        apiSecrets: stackEnvironment.apiSecrets,
        proxyEnvironment: stackEnvironment.proxyEnvironment,
        proxySecrets: stackEnvironment.proxySecrets,
        collectorEnvironment: stackEnvironment.collectorEnvironment,
        collectorSecrets: stackEnvironment.collectorSecrets,
        runnerEnvironment: stackEnvironment.runnerEnvironment,
        runnerSecrets: stackEnvironment.runnerSecrets,
      },
    })
  },
})
