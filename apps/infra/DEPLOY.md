# Deploying BoxLite

Three tools, three config files, two clouds, one description of the stack.

This sits beside the incumbent `sst.config.ts` / `deployment/sst.ts` rather than
replacing it. Both describe the same app and the same stage, and every module
keeps the incumbent's logical resource names — so the new path adopts the
existing state instead of building a second set beside it. That is what makes
the cutover a diff to read rather than a migration to perform:

```
npm run mdeploy -- --stage dev --diff
```

## The shape

```
apps/infra/
  mstage/                  shared: sign-in, stage coordinates, the config store, state repair
  mbuild/                  shared: build, publish, promote and verify images
  mdeploy/                 BoxLite's stack, and the two engines that apply it
    sst.config.ts          AWS: the modules, composed for SST
    pulumi/program.ts      GCP: the same modules, composed for Pulumi
    globals.d.ts           what both engines inject, declared so tsc can see it
    src/deploy.ts          which engine — resolved once, from one field
    src/plan.ts            what each module builds, and what it needs first
    src/stack-env.ts       what one deploy reads out of the environment, for both engines
    src/api-environment.ts what the control plane container reads
    stack/                 what each module needs, described without a cloud
    stack/providers/aws/   how AWS answers it
    stack/providers/gcp/   how GCP answers it
  mstage.config.json       which stages exist, where they live, and what the store may hand out
  mbuild.config.json       what to build, and which repository receives it
  mdeploy.config.json      what shape to deploy into
```

`mstage` and `mbuild` know nothing about BoxLite. They are the same code
`boxlite-backoffice` runs, with a different JSON file beside them. `mdeploy` is
the outlier, and not because its code is repository-specific: its contracts name
no cloud and no application, and what belongs to BoxLite is the *set* of modules
— that there is a control plane, a box proxy, a collector and a fleet of hosts
with nested virtualization.

## Which file holds what

A value belongs in `mdeploy.config.json` when changing it changes the
infrastructure, and in `mstage.config.json` when changing it changes what a
running thing reads. `STACK_DOMAIN` is a store value: moving a stage to another
domain changes no resource shape. `runners.size` is `mdeploy`'s: it decides
which machine family a host is created from, and on GCP whether nested
virtualization is available at all.

Neither file holds a secret, and neither holds anything one deploy decides — an
image tag and a runner binary's checksum come from the invocation.

## Two clouds, one stage at a time

`mstage.config.json` declares `home` for the repository, and per stage where one
differs:

```json
"stages": {
  "dev":     { "region": "ap-southeast-1" },
  "prod":    { "region": "ap-southeast-1", "protect": true },
  "gcp-dev": { "home": "gcp", "region": "asia-southeast1", "project": "boxlite-gcp-dev" }
}
```

That one field picks the store backend, the identity, the provider bundle the
stack is built from, the engine that applies it, and the registry kind images
are published to. Nothing above those seams branches on a cloud.

The engine differs because of state, not preference. SST keeps state only in S3,
R2 or a directory on the machine, so a GCP stage deployed through SST would need
an AWS bucket — and therefore an AWS credential — to deploy into Google.
Pulumi's own backend takes `gs://`, so a GCP stage keeps its state in the
project it deploys into and needs no second cloud in the deploy path at all.

## Where the clouds genuinely differ

Written down rather than smoothed over:

| Module | AWS | GCP |
|---|---|---|
| network | VPC, EC2 NAT, security groups | VPC, Cloud NAT, firewall rules keyed on service accounts, Private Service Access |
| database | RDS, password copied into Secrets Manager | Cloud SQL private IP, password generated into Secret Manager |
| cache | ElastiCache | Memorystore |
| storage | S3, grant scoped by ARN prefix | Cloud Storage, project role bounded by a CEL condition |
| cluster | one ECS cluster | nothing — Cloud Run has no cluster |
| clickhouse | EC2 + retained EBS, schema reconciled over SSM | GCE + retained disk, schema applied at boot |
| mail | SES, DKIM and DMARC verified | nothing — Google has no sending service; a relay is named or mail is off |
| api | ECS behind an ALB and a CDN | Cloud Run behind a global load balancer |
| edge | ECS behind an NLB with `443/tls` | **a managed instance group** behind a passthrough load balancer |
| runners | EC2, `cpuOptions.nestedVirtualization` | GCE, an N2 family, `minCpuPlatform` and `enableNestedVirtualization` |
| alarms | CloudWatch on emitted counters | alert policies on log-based metrics |

Two of those are worth reading the file for.

**The proxy is not a Cloud Run service.** It terminates TLS itself so it can
read the SNI name — `<port>-<boxid>.<domain>` — and decide which runner holds
that box. Google offers exactly one load balancer that does not terminate on the
way in, and Cloud Run cannot be a backend of it. So the GCP proxy runs on
container-optimised VMs, which costs a machine per zone the AWS side does not
spend. `stack/providers/gcp/edge.ts` says so at the top.

**A runner needs three things on GCP that it needs none of on AWS**: a machine
family that can nest (E2 cannot), `minCpuPlatform` of Haswell or later, and
`enableNestedVirtualization` set explicitly — plus the guest's `/dev/kvm` made
readable by the account the runner runs as. Those are exactly what
`scripts/deploy/gcp/create-instance.sh` and `setup-kvm.sh` have been doing by
hand for a developer's own box host; `stack/providers/gcp/runners.ts` and
`stack/runner-boot.ts` make them part of a deploy.

## Commands

```
npm run mstage login                                   who am I, on this stage's cloud
npm run mstage env list     -- --stage dev             names only
npm run mstage env list     -- --stage dev --values    values, asked for explicitly
npm run mstage env digest   -- --stage dev             expect: / got:
npm run mstage env set      -- --stage dev --digest KEY=VALUE
npm run mstage state unlock -- --stage dev             what a killed deploy left
npm run mstage state edit   -- --stage dev             the checkpoint, in $EDITOR

npm run mbuild publish -- --tag <sha> --stage dev      build and push every artifact
npm run mbuild promote -- --tag <sha> --from dev --to prod
npm run mbuild verify  -- --tag <sha> --stage dev      does this stage hold this commit

npm run mdeploy -- --plan                              the batches, derived from the graph
npm run mdeploy -- --stage dev --diff                  read this before the first apply
npm run mdeploy -- --stage dev
npm run mdeploy -- --stage dev --remove --confirm
```

## What is verified, and what is not

| | |
|---|---|
| mstage — sign-ins, the store, digests, object versions, state repair | 327 tests |
| mbuild — addresses, the publish sequence, the scan gate, the workflow | 58 tests |
| mdeploy — the plan, both configs, the environment, the wiring, both bundles | 82 tests |
| the incumbent stack and its release guards, plus `bootstrap/gcp.ts` | 522 tests |
| mstage, mbuild **and mdeploy** typecheck | `tsc` clean, without `sst install` |
| every GCP provider, applied | **never run** — no project, no network, no billing |

`mdeploy` being inside the typecheck is the one place this diverges from the
repository the pattern came from, where it was left outside. `globals.d.ts`
declares what both engines inject, so a contract that a provider stopped
satisfying is a compile error rather than a runtime one. What it does not check
is a resource argument's spelling — that needs the providers' own types, and the
file says so.

## What is left

- **A GCP account.** The deploy path is written, typechecked and tested, and
  `bootstrap/gcp.ts` now creates everything an identity needs beyond the
  project itself — the enabled APIs, the state bucket, the workload identity
  pool, the deployer and publisher service accounts, the Artifact Registry
  repository — and wires `GCP_WORKLOAD_IDENTITY_PROVIDER`, `GCP_DEPLOYER` and
  `GCP_IMAGE_PUBLISHER` into GitHub the same way the AWS half already wires
  its own role ARN. What is still manual is the project and its billing
  account: nothing here creates either, and no bootstrap can — a project
  needs a parent to be created under, and enabling billing needs a billing
  account already linked to it.
- **The batch pipeline.** `src/plan.ts` and `--module` are complete and tested,
  and `sst` targeting aborts with `Duplicate resource URN`
  ([pulumi/pulumi#24303](https://github.com/pulumi/pulumi/issues/24303), open).
  The workflow deploys one apply until that lands.
- **Retiring the incumbent.** `deploy-infra.yml`, `deploy-release.yml` and
  `build-apps-api-image.yml` still run. Two publishers writing immutable tags
  into one repository is a race that reads as a broken build, so retiring them
  is the step after the first green `mdeploy` dispatch.
- **The application on GCP.** Deploying the stack is not the same as running on
  it: the API's object-storage client reaches for STS, and the runner's volume
  mount is Mountpoint for S3. The deploy is portable ahead of the thing it
  deploys.
