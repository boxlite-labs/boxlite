# Infrastructure security

A stage's configuration is loaded into the environment just before SST starts, from the stage's SST
secret store; the Cloudflare provider credentials come from SSM or the job's Environment secrets,
since reading that store initializes the Cloudflare provider. No workflow writes configuration to
disk, and only keys named by the store's own `BOXLITE_STAGE_CONFIG` manifest are applied, so a secret
written under any other name cannot reach a deploy's environment.

The application secrets — `OIDC_CLIENT_ID` and the others declared as `sst.Secret` — live in that
same store but are **not** part of that hydration. SST resolves them itself, so they never enter the
deploy wrapper's own `process.env`; the stack still passes them to the services that need them, which
is how they reach a deployed application's environment.

Two separate things keep them intact. `sst secret load` merges rather than replacing, and bootstrap's
payload excludes those names outright (`APP_SECRET_NAMES`), so a load cannot overwrite a value the
operator was just prompted for. The manifest is not what protects them — it governs which stored keys
may be hydrated, not what the load writes.

A stage's deploy role reaches its own stage's **SST secret store**, and no other stage's. One bucket
and one parameter tree hold every stage's, so the role's `s3:*`/`ssm:*` grant on `*` used to let a job
bound to the dev Environment run `sst secret list --stage prod`. Those two are now scoped to this
stage's state prefixes, its passphrase, and the buckets the stack owns.

That is a claim about the SST store specifically, which is where a stage's configuration lives. AWS
Secrets Manager is now scoped the same way: everything except the two calls that take no resource
(`ListSecrets`, `GetRandomPassword`) is limited to `secret:boxlite-<stage>-*`.

Three things in the SST store are shared by construction, and none of them is a stage's configuration:

- `ListBucket` stays at bucket scope, because SST enumerates before it reads. Other stages' key
  *names* remain visible; their contents do not.
- `_fallback` secrets belong to the app rather than a stage, so they cannot be stage-scoped and are
  read-only — a deploy consumes a fallback, setting one is a deliberate operator action.
- `/sst/bootstrap` names the buckets every stage shares and is read before SST knows which stage it
  is running, so it is read-only for the same reason.

Four shared writes remain, and none is narrowed by this change:

- Deployment assets share a single `sst-asset-*` bucket with no stage in the key, so every stage has
  `PutObject`, `DeleteObject` and `DeleteObjectVersion` over all of it. SST derives each key from the
  content hash, which makes an ordinary deploy's writes idempotent — but that is SST's behaviour, not
  a constraint the policy imposes: the grant permits writing arbitrary bytes to any key in the bucket,
  or deleting one another stage's next update expects. It stays because `sst remove` needs the delete,
  and the blast radius is bounded by assets being rebuilt from the checkout rather than recovered.
- `boxlite-volume-*` buckets carry no stage in their names, so `s3:*` over that prefix reaches every
  stage's volumes. Scoping it means renaming the buckets to carry the stage, across the runtime
  boundary, the stack, and buckets that already exist.
- `ssm:SendCommand` on `instance/*` is the widest grant the deploy role holds: an instance ARN
  carries no stage, so a job bound to one stage can run a shell command on any instance in the
  account. Narrowing it needs a Condition on an instance tag the Runner launch template sets, which
  can only be verified against real instances.
- SES identity management reaches `identity/*`: an identity ARN carries the sender domain and no
  stage, so a job bound to one stage can create or delete another stage's mail identity — deleting
  one silently stops that stage's mail until it is verified again. It does not include sending:
  `ses:SendRawEmail` is deliberately absent here and belongs to the send-only IAM user bootstrap
  provisions. Scoping this means passing the stage's `MAIL_DOMAIN` into the role template, which ties
  the role's shape to a value the stack reads at deploy time and re-runs bootstrap whenever it
  changes. The stage's SES *configuration set* is scoped, because `stack/mail.ts` names it
  `boxlite-<stage>-mail` instead of letting Pulumi autoname it.

The first two predate this change, the third is how Runner upgrades have always been delivered,
and the fourth arrives with outbound mail; they are listed because the paragraph above would
otherwise read as a stronger guarantee than the policy gives.

The three grants that used to reach other stages are now scoped by `${GitHubEnvironment}` (#1255).
`secretsmanager:ListSecrets` is the residue: it takes no resource, so a stage can still enumerate
other stages' secret *names* and metadata — not their values.

- `ManageBoxLiteRoles` / `ManageBoxLitePolicies` and the instance-profile grants moved from
  `boxlite-*` to `boxlite-<stage>-*`, so a job bound to one stage can no longer rewrite or delete
  another stage's SST-created runtime roles.
- `secretsmanager:*` moved from `*` to `secret:boxlite-<stage>-*`.
- `AssumeBoxLiteRuntimeRoles`, in the runtime permissions boundary rather than the deploy role, moved
  from `role/boxlite-*` to `role/boxlite-<stage>-*` — otherwise a task could assume another stage's
  runtime role.

A wrong pattern fails a deploy with AccessDenied and needs the bootstrap stack redeployed to clear,
so what the patterns must cover was read out of SST rather than assumed
(`.sst/platform/src/components/component.ts`). Two conditions gate the `<app>-<stage>-` prefix: the
resource type must be in `namingRules`, and it must be created as a component's *child*, because the
renaming transform is registered in the Component constructor. Roles (`:257`) and secrets (`:295`)
qualify; instance profiles and managed policies are on the skip list (`:142-143`) and are never
prefixed. So `boxlite-dev-ApiExecutionRole-*` is covered, while a role declared at stack root
autonames (`RunnerRole-1115ba6`) — as it did under `boxlite-*` too, so narrowing costs it nothing.

The stack's secrets divide the same way: `DatabaseProxySecret` and `CacheProxySecret` are component
children and land inside the pattern; `GhcrPullToken` is declared at stack root and is named
explicitly in `stack/runners.ts` so that it does. Without that name it would autoname and the
narrowed grant would deny it — the deploy would fail the first time `GHCR_TOKEN` is set.

`DenySelfPrivilegeEscalation` stays deliberately cross-stage (`boxlite-*-github-deploy`,
`boxlite-*-runtime-boundary`). Narrowing the Allows makes it redundant on paper; it is kept as the
backstop for the next grant that widens.

That policy has not run a real deploy yet. Four of its grant groups were derived from live listings,
and two — Pulumi's provider describe/list calls and the `ssm:SendCommand` document targets — are
reasoned. A gap surfaces as AccessDenied on a preview, and clearing it means redeploying the bootstrap
stack with a wider policy, so preview with `apply=false` before any apply.

Every SST-created IAM role receives the stage runtime permissions boundary through the global role
transform. `npm run bootstrap` (bootstrap/aws.ts, reconciling the checked-in documents in
bootstrap/aws/) creates the deployment boundary and the GitHub OIDC trust used before SST can
deploy anything.

Runner instances are protected resources. The mandatory policy pack validates their identities,
lifecycle options, and normalized state baseline during real previews and deploys. Runner AMI and
user-data changes are intentionally ignored; binary updates use the serial SSM workflow instead of
replacing stateful hosts.

Pulumi event logs may contain provider inputs. The deployment facade removes stale logs before SST
and newly created logs immediately after SST, including failure and signal paths.
