# mstage

A modular replacement for the parts of SST this platform actually depends on:
bootstrap discovery, the S3 state and secret store, and the AWS sign-in that all
of them need first.

It exists because reading one secret currently costs an entire SST project
init — discovering `sst.config.ts`, unpacking the embedded platform, installing
`@pulumi/aws`, evaluating the config through esbuild and node — before the four
AWS calls that do the work. `apps/api/Dockerfile` warms that machinery at image
build time purely to make `sst secret list` runnable inside a container.

## Invocation

Run from `apps/infra`, which owns this tool. Nothing is wired into the
repository root.

```
npm run mstage <module> <command> -- [--stage <stage>] [options] [-- <inner command>]
```

Everything to the right of the first `--` reaches mstage untouched. Everything to
its left belongs to npm, which claims any `--flag` for itself: `--stage dev`
written there becomes `npm_config_stage=true` plus a stray `dev` positional,
silently shifting the command. mstage detects that and refuses rather than acting
on an invocation nobody typed.

| Module  | Commands                                  | Needs `--stage` |
| ------- | ----------------------------------------- | --------------- |
| `login` | `aws`, `github`, `auth0`, or none for all | no              |
| `aws`   | `whoami`, `region`, `exec`                | yes             |
| `env`   | `list`, `set`, `digest`, `del`            | yes             |
| `state` | `unlock`, `edit`                          | yes             |

`npm run mstage <module> -- --help` lists what that module accepts, generated from
the same table the dispatcher runs on, so it cannot describe a command that is
not there.

```bash
npm run mstage login
npm run mstage login github
npm run mstage login -- -f            # sign in again first, then report
npm run mstage aws whoami -- --stage dev
npm run mstage aws exec -- --stage dev -- aws s3 ls
npm run mstage env list -- --stage dev
npm run mstage env set -- PORT=8080 TIMEOUT=30 --stage dev
npm run mstage env set -- SHAPE='{"a":"b", "c":"d"}' --stage dev --json
npm run mstage env set -- SMTP_PASSWORD --stage dev < password.txt
npm run mstage env set -- --stage dev --select-group deploy < stage.json
npm run mstage env digest -- --stage dev
npm run mstage env del -- OLD_KEY --stage dev
npm run mstage env del -- A B C --stage dev
npm run mstage state unlock -- --stage dev
npm run mstage state edit -- --stage dev
```

## What is here, and what is not

Everything in mstage is shared: sign-in, stage configuration, the AWS identity a
stage resolves to, and that stage's environment in the SST state bucket. mstage
obtains access, checks it, and reads and writes what a stage is configured with.

Spending that access is not here, and neither is any account of how it gets
spent. Deployment differs per repository — machine shapes, images, rollout
gates — so each repository has its own tool that asks mstage for a session, an
identity and a stage environment, and then does its own work. In this repository
that is `apps/infra/mdeploy`, which documents itself.

`state` is the edge of that line rather than a crossing of it. mstage does not
deploy, take the lock or write a checkpoint; it repairs the two objects a deploy
that stopped halfway left in the same bucket, which no deploy can do for itself
because it is exactly those objects that stop the next one from starting.

What mstage offers such a tool, besides the modules above, is its own parser: a
caller passes the options it owns (`mdeploy` has `--local-env`) and mstage parses
them for that call without listing them in `mstage --help`. Two tools then read
one command line the same way, and neither advertises the other's switches.

## mstage.config.json

Lives in `apps/infra`, beside the `sst.config.ts` it describes, and is what
makes a stage name mean something. It is found by walking up from the working
directory, or named outright with `MSTAGE_CONFIG`.

```json
{
  "app": "boxlite-backoffice",
  "home": "aws",
  "login": {
    "aws": { "required": true },
    "github": { "required": true },
    "auth0": { "required": false }
  },
  "env": {
    "selectGroup": {
      "deploy": ["BACKOFFICE_DOMAIN", "BACKOFFICE_BOXLITE_ADMIN_BASE_URL", "BACKOFFICE_STAGE_CONFIG_DIGEST"]
    },
    "digest": { "key": "BACKOFFICE_STAGE_CONFIG_DIGEST", "group": "deploy" }
  },
  "stages": {
    "dev": { "region": "ap-southeast-1" },
    "prod": { "region": "ap-southeast-1", "protect": true }
  }
}
```

`login` names which sign-ins this repository needs. mstage knows how to check and
obtain a session for each; only the repository knows which it cannot work
without — boxlite-commerce needs AWS alone. An optional provider is still
checked and reported, but does not fail the command.

`env.selectGroup` declares the named subsets of the store that may leave it, and
`env.digest` names the key that fingerprints one of them. Both are described
below; `env.digest.key` must be a member of the group it describes, so the
fingerprint travels with what it fingerprints.

One group name means more than the others. `env.selectGroup.secret` marks the
keys whose value is the _address_ of a secret rather than the secret, which is
what lets a workload be handed one by reference; see "Secrets by reference"
below. It names no consumer of its own: it says how a key travels, and whichever
group also names that key is what says who reads it. The two are orthogonal, so
a marked key appears in both — and one that no other group names is refused,
because a mark on a key nobody receives is a mark on nothing.

A stage that is not declared here is a typo, not a new environment. Stage names
follow SST's own constraint (`[a-zA-Z0-9-]+`) because mstage reads and writes the
same S3 keys. `region`, `project`, `roleArn` and `protect` are each optional:
`roleArn` is assumed on top of the resolved credentials; `protect: true` requires
`--confirm`; `project` is where a GCP stage names the project it lives in, which
is declared because Google's clients cannot be opened without one.

No stage declares an AWS account. The account is whichever one the resolved
credentials belong to, and a caller that has to name it in an ARN — `bootstrap`
rendering an IAM document, `mbuild` naming an ECR host — reads it back from
`whoami`. Declaring it as well would be a second copy of something already
known, kept in step by hand.

`dev` and `prod` are declared, both in one region. `prod` is protected because a
write there is live.

How a stage deploys is deliberately absent — that belongs to `mdeploy`.

## The stage environment

`mstage env` reads and writes the store SST calls secrets, which on this platform
holds a stage's whole configuration. Four calls, and no project init:

```
SSM  /sst/bootstrap                 → the state bucket's name
S3   secret/<app>/<stage>.json      → the encrypted map
SSM  /sst/passphrase/<app>/<stage>  → the key
AES-256-GCM                         → the map
```

Those paths are SST v3.19.3's own (`pkg/project/provider/aws.go:541` and `:545`),
because these objects are shared with it: a deploy still writes what this reads,
and on a stage SST has already written, `sst secret set` and `mstage env set` are
interchangeable. On one it has not, `mstage env set` refuses: the passphrase is
generated by SST on first use, with `Overwrite=false` and a description reading
"DO NOT DELETE STATE WILL BECOME UNRECOVERABLE", and inventing that key as a side
effect of setting a value is not a decision mstage should make.

The bucket name never appears in output — not on success, not in an error. Its
twelve random characters are chosen at bootstrap and recorded only in
`/sst/bootstrap`, so they are the one thing keeping these objects unaddressable;
a log is read by more people than the account is. Messages name the stage, or
the object's key, both of which the caller already supplied.

`env list` prints names only. `sst secret list` prints every value with sst's
stdio inherited, which was already more than "lists what is set" and became far
more once the store started holding whole stage configurations: one command drops
every token and private key into scrollback. Values need asking for:

```bash
npm run mstage env list -- --stage dev                        # names only
npm run mstage env list -- --stage dev --values               # and their values
npm run mstage env list -- --stage dev --select-group deploy   # one declared group
npm run mstage env list -- --stage dev --select-group deploy --json  # the same, as JSON
npm run mstage env list -- --stage dev --json                 # the whole store, as JSON
```

An empty store is a failure rather than an empty answer, which is what SST
reports too: a caller that cannot tell the two apart would deploy with no
configuration at all.

A group is `env.selectGroup.<name>` in `mstage.config.json`, so adding a key to an export
is a reviewable edit to a file rather than a longer command line — which is the
only reason exporting is safe at all. A group naming a key the store does not
hold is an error, not a short answer: a deploy handed a silently incomplete
environment fails later, somewhere that does not mention the missing key. Every
value prints as the one string it is — `KEY=A,B,C,D` is seven characters and not
a list — so a value that merely contains a comma, a JSON document among them,
comes out whole.

A group may name no keys at all. A repository that gives each of its services a
group needs to say that one of them reads nothing yet, and the alternatives are
worse: a placeholder key, or leaving the service undeclared where nothing
holding the config against the services can see it. `env.digest` is unaffected,
because the group it fingerprints must carry the digest key and so cannot be
empty.

`env set` takes assignments, as many as fit on the line, and lands them in one
write — the store is a single object, so writing per key would cost a round trip
each and widen the window in which a concurrent writer loses somebody's change.
`\n`, `\r`, `\t`, `\\` and `\"` are expanded, the same sequences SST's own file
loader accepts (`cmd/sst/secret.go:199-207`), because a shell has no other way to
put a newline in an argument.

`--json` says the values on the line are JSON documents:

```bash
npm run mstage env set -- SHAPE='{"a":"b", "c":"d"}' --stage dev --json
```

Each one is parsed, refused if it does not parse, and stored as JSON writes it
rather than as the shell typed it — so one value is one stored string, and
re-typing the same object with different spacing is not a change and does not
move a group's digest. A document carries its own escapes, so the expansion
above does not run on it: `{"key":"a\nb"}` already means a newline, and expanding
it first would leave a raw newline inside a JSON string, which is not JSON at
all.

Asked for rather than detected from the value. A value beginning with `{` is
usually a document, but `KEY={VALUE}` is two words and a pair of braces, and
guessing would turn it into a refusal; the caller who means a document can say so
in a word. Without the flag every value is a line of text, exactly as before.
`--json` describes a value, so a line that carries none — a piped document, or
`--digest` on its own — is refused rather than accepted with a flag that did
nothing.

Two other forms exist for values a command line cannot carry. A lone `KEY` with
no `=` reads its value from stdin, whole and with its trailing newline kept,
which is what `sst secret set` stores for the same input. No arguments at all
reads a JSON object of them — the shape `env list --json` prints, so a store
exported from one stage loads into another unedited. JSON is the only accepted
document format because it carries a newline with no escape convention to learn;
for the same reason its values are used exactly as parsed, with no second pass of
expansion.

A store holds strings, so a string value is stored as it was parsed and anything
else JSON can hold is stored as the JSON text of that value — the same thing
`--json` does for one value on a command line, so a document says the same thing
whichever way it arrives and a nested object needs no escaping to survive being
written into a file:

```json
{ "KA": { "A": "one", "C": "two" }, "MANY": ["x", "y"], "PORT": 8080 }
```

stores `{"A":"one","C":"two"}`, `["x","y"]` and `8080`. A list is stored as the
list it is rather than joined on a separator its own elements may contain.
`null` is the one refusal: a key whose stored value is the text `null` is
nobody's intention, and a key that should not be there at all is `env del`.

> [!IMPORTANT]
> A store exported before this change does not load back into one. The old
> `env list --json` printed any value holding a comma as an array, and the
> piped form joined it back on the comma — lossless while both halves of that
> convention existed, and neither exists now. An array in an old file therefore
> loads as the JSON text of that array: a value stored as `x,y,z` comes back as
> `["x","y","z"]`. The write names every list it stores, so this is not silent —
> but it is still a rewrite, and only you know which of those lists were
> values. `BACKOFFICE_PLATFORM_CONFIG` is the shape
> that stings — a JSON document the old export split on the commas inside it,
> in a group `env.digest` certifies. No single command can both rewrite and
> certify — `--digest` never reads a piped document — but two can, and the
> second would put the fingerprint behind the rewrite. Export the stage again
> rather than loading a file written before this.

```bash
npm run mstage env set -- PORT=8080 TIMEOUT=30 --stage dev
npm run mstage env set -- PRIVATE_KEY --stage dev < key.pem
npm run mstage env set -- --stage dev < stage.json
npm run mstage env set -- --stage dev --select-group deploy < stage.json
```

`--select-group` narrows a document to the keys one group names, so a whole store can be
piped in and only the reviewed part of it lands. What it drops it names, because
a key that vanishes silently looks like a key that was written, and a document
with nothing in the group is refused rather than reported as a write that did not
happen.

No value is ever echoed back. What is reported is names, and whether each was
added, replaced, or already held that value.

### Secrets by reference

A stage's store holds its configuration, and everything in it is delivered to
whatever consumes it as a value. For a secret that is more than it needs to be:
the value ends up in a task definition or a service revision, where anyone who
may describe one can read it and where every revision ever registered keeps its
own copy.

`env.selectGroup.secret` is the other way. Its keys hold the address of a secret
kept in the cloud's own secret store, and what resolves the address is the
platform the workload runs on — an ECS `secrets` entry, a Cloud Run
`secretKeyRef` — as the container starts. The secret itself never travels: not
through this store, not through the deploy, not into a task definition.

An address is a one-field JSON document, so it is written with the JSON form of
`env set`:

```bash
# an AWS stage: a Parameter Store SecureString, named by ARN
npm run mstage env set -- --stage dev --json \
  BACKOFFICE_OIDC_CLIENT_SECRET='{"address":"arn:aws:ssm:ap-southeast-1:123456789012:parameter/boxlite-backoffice/dev/oidc-client-secret"}'

# a GCP stage: a Secret Manager secret, named as a resource
npm run mstage env set -- --stage dev --json \
  BACKOFFICE_OIDC_CLIENT_SECRET='{"address":"projects/boxlite-dev/secrets/oidc-client-secret"}'
```

Which form is accepted follows `home`, so an address for the other cloud is
refused rather than stored to fail at the next deploy. On AWS both a Parameter
Store parameter ARN and a Secrets Manager secret ARN are addresses, because both
are what that reference channel resolves and this platform's older secrets live
in Secrets Manager. A full ARN and not a bare parameter name: ECS accepts a bare
name only for a parameter in the task's own region and account, and an ARN is the
form a reviewer can read the region and the account out of. On GCP the address
carries no version, because Cloud Run takes the version as its own field and an
address that named one would be declaring it twice.

`--json` is how the address is written above, but it is not what makes the key an
address: `env.selectGroup.secret` is. The flag decides how a value is read; the
group decides what it has to be, so an address typed without the flag is checked
just the same — it is simply stored as typed rather than as JSON writes it.

The value itself is refused where an address belongs, at the write and again at
the deploy — the store is also writable by `sst secret set`, so being sure once
is not being sure. No refusal quotes the value: the mistake it exists to catch is
the secret written in place of its address, and a message that echoed it would
put it in the terminal the whole arrangement is keeping it out of.

One thing to confirm before the first key moves, on AWS. The ECS agent resolves
these with the task's execution role, whose inline policy SST writes with
`ssm:GetParameters` and `secretsmanager:GetSecretValue` — but `mdeploy` attaches
the account's `boxlite-role-boundary` to that role, and a boundary caps what a
policy allows. That document is not in this repository, so what it permits
cannot be read from here. The database password already reaches a container
through the same channel as a Secrets Manager ARN on a stage that runs, so that
half is exercised; a Parameter Store ARN is not, and a parameter under a
customer-managed KMS key would need `kms:Decrypt` besides. A boundary that
refuses the read fails every task at start.

Nothing about the reference is mstage's to arrange beyond that. Creating the
parameter or the secret, and granting the workload permission to read it, belong
to whoever owns the cloud; a deploy tool reads the group and hands each address
to the platform — in this repository `mdeploy` does, through
`env.selectGroup.secret` and nothing else. Adopting it for a key already in the
store is three steps in order: put the secret in Parameter Store or Secret
Manager, add the key to `env.selectGroup.secret` in `mstage.config.json` —
leaving it in the service group that delivers it, which is what still says who
reads it — then `env set` its address. The middle step alone leaves the next
deploy refusing, by name, a key that still holds a value.

### The fingerprint

`env.digest` in `mstage.config.json` names a key and one `env.selectGroup`:

```json
"env": { "digest": { "key": "BACKOFFICE_STAGE_CONFIG_DIGEST", "group": "deploy" } }
```

`env set --digest` writes that key alongside the assignments, in the same write,
over the group as it will be rather than as it was — a digest of the previous
configuration would certify the wrong thing. The digest key is a member of the
group it describes, so a consumer that reads the group has it without a second
lookup, and it is the one group member allowed to be absent when the digest is
being derived: a store that has never held one can still be given its first.
`--digest` on its own recomputes over the store as it stands, which is how a
group edited by other means gets certified again.

`env digest` checks it: it prints what it expects and what is stored, and exits
non-zero when they differ. That is the whole point — the gate refuses when the
configuration moved after something was built against it. A group member that is
missing entirely refuses too, through the same exit code; missing and mismatched
are the same answer.

`env del` removes keys — as many as fit on the line, in one write, for the
reason `set` writes once: the store is a single object, so removing them one at a
time would cost a round trip each and widen the window in which a concurrent
writer loses somebody's change. A key that was not there is reported and is not
an error, and when none of the names was there nothing is written: the store ends
up in the state that was asked for either way, and a caller cleaning up after a
rename should not have to know which half already ran. A name repeated on the
line is refused rather than reported twice for one removal.

`env del --digest` keeps `env.digest.key` true, which for a removal means
refusing one that would falsify it. Naming any member of the certified group —
the digest key included, since it is one — refuses the whole command before
anything is removed. The flag writes nothing: a removal it allows touches no
member of that group, so the stored fingerprint still describes it, and a
removal it refuses could not be mended by recomputing while the group still
names what went. `set --digest` is the half that writes. Removing a group member
is still allowed without the flag, which is how a group shrinks: the next deploy
then refuses by name until `mstage.config.json` catches up.

A name being written must match SST's own rule for one it can set,
`[A-Z][a-zA-Z0-9_]*` (`cmd/sst/secret.go:363`), because a name SST cannot set is
one SST cannot read back. `del` does not apply that rule: a store mstage can open
holds whatever is in it, and refusing to remove a key over how it is spelled
would leave it there. Both commands need `--confirm` on a stage marked
`protect: true`.

### One layer, not two

SST keeps a second section per app, `secret/<app>/_fallback.json`, applied
beneath whichever stage was asked for. mstage does not read or write it. Neither
`boxlite-backoffice` nor the platform's `boxlite` store has ever had one, and a
layer that is always empty still costs every reader a merge and every writer a
decision about which layer it meant — plus, while it existed here, an
inconsistency between the two paths that computed the digest. A value shared by
two stages is written to both.

Anything written there by `sst secret set --fallback` is therefore invisible to
`env list`, and a group that names such a key fails as
`the store is missing <key>` rather than exporting a short environment.

One property is worth knowing before relying on any of this:

- **A write replaces the whole object.** The store is one encrypted document, so
  every `set` and `del` is a read-modify-write, exactly as `sst secret set` is.
  Two writers racing lose one of the two changes silently — including a deletion,
  which comes back. SST has that property too and mstage does not add locking on
  top of a store it shares, so batch changes from one place.

### Reading a group from code

A group is what a program asks for too, through the one function that answers
for it:

```js
import { selectGroup } from 'mstage/select-group'

Object.assign(
  process.env,
  await selectGroup({
    group: 'api',
    stage: process.env.BACKOFFICE_STAGE,
    region: process.env.AWS_REGION,
    versionId: process.env.BACKOFFICE_STAGE_SECRETS_VERSION,
  }),
)
```

`selectGroup` returns the group's keys and values and does nothing else with them —
whether they belong in `process.env`, in a child process, or in a file is the
caller's decision, and a library has no business making it. `apps/api/src/main.ts`
assigns them into its own environment at startup; `mdeploy` hands them to `sst`
as a child environment instead. Both name a group; neither carries a list of
keys, which is the point: `env.selectGroup` is the only place that says what may leave
the store, so nothing has to be kept in step with it.

`versionId` reads the object as an earlier moment saw it. A deploy records the
version it shipped (`currentVersion`) and passes it here, so a task that starts
again hours later reads the configuration the deploy was built against rather
than whatever the store holds by then; a version that has since been deleted is
an error rather than a quiet fall back to current.

The group must be complete: a key it names that the store does not hold is an
error, so a process never starts on a silently short environment. Which config
file answers is found by walking up from the working directory, or named
outright with `MSTAGE_CONFIG` — which is how a container that ships only `dist`
points at it (`apps/api/Dockerfile`).

## What a stopped deploy leaves

A deploy locks the stage, rewrites the deployment checkpoint as it goes, and
drops the lock on its way out. One that is killed never reaches the last step: a
cancelled workflow, an expired runner or a closed laptop leaves the lock behind
and leaves the operations it was in the middle of recorded as pending. The next
deploy then refuses twice over — the stage looks busy, and Pulumi will not plan
over operations whose outcome nobody observed.

```bash
npm run mstage state unlock -- --stage dev   # the lock nobody released
npm run mstage state edit -- --stage dev     # the operations nobody finished
```

`unlock` prints what held the lock — the command, the run id and when it was
taken — before removing it, because the one thing it cannot tell is whether that
deploy is still running somewhere. It then reads the lock again and removes it
only if it is still the one that was named, so a deploy that starts in between
does not lose the lock it is holding. `edit` opens the checkpoint in `$EDITOR`
(`code -w` and the rest work; the default is vim, as it is in SST), says how many
pending operations are in it first, and refuses to open while a lock is held.
Deleting the entries from `checkpoint.latest.pending_operations` is the edit
that unsticks a stage.

What comes back is checked before it is stored, and three things can refuse it.
A file that no longer parses, or that lost the `{"version":3,"checkpoint":{…}}`
wrapper, would describe a stage with no resources rather than the stage as it is.
A lock taken while the editor was open means a deploy started meanwhile. And the
stored bytes are compared against the ones the editor was given, because a
deploy can take the lock and drop it again inside one editor session. On any of
the three the copy is kept and named: that edit is the operator's work and the
only place it exists. The comparison is not atomic and is not sold as one — it
closes the window that is minutes long, not the one that is milliseconds long.

Both act on the engine's own objects, and which those are is the backend's
business rather than the command's. On an AWS home they are SST's —
`app/<app>/<stage>.json` and `lock/<app>/<stage>.json`, beside the store's
`secret/<app>/<stage>.json` — and these do what `sst unlock` and `sst state edit`
do. On a GCP home the engine is Pulumi itself, which keeps its checkpoint at
`.pulumi/stacks/<app>/<stage>.json` and its locks as a _directory_ of files, one
per operation holding the stage; `unlock` refuses rather than guessing when it
finds more than one, and removes every one of them when it acts.

They are here rather than run through either CLI because both have to load a
stack config to do it, and which stack a repository deploys is that repository's
business; the objects are ones mstage already reads.

Clearing a pending operation is not the same as knowing what became of the
resource it names. The operation was interrupted, so the cloud may hold something
the checkpoint does not, or the reverse. The edit makes a stage deployable again;
a refresh is what makes it accurate, and it belongs before the next deploy.

## Credentials

Sign in however this machine signs in — `aws login`, `aws sso login`, whatever —
and mstage picks up the result. It resolves through the AWS SDK's default
credential chain and nothing else: no `--profile`, no `MSTAGE_AWS_PROFILE`, no
profile field in `mstage.config.json`, and no translation of the SDK's errors. A
failure surfaces exactly as the SDK reported it.

mstage does not second-guess which account the chain reaches. `aws whoami`
prints the account and principal it found, and that is the way to check before
spending anything; `aws region` calls nothing at all, resolving the region
locally from the table below.

`mstage login` applies the same rule to GitHub and Auth0 — it reads the session
`gh auth login` and `auth0 login` left behind, and repeats those CLIs' own
message when there isn't one. All three providers are documented in `--help`
whether or not this repository declares them, because mstage is shared and does
not define the set; naming one `mstage.config.json` does not declare is refused,
and a missing session for a declared, required provider is what fails the
command.

It signs nobody in unless asked. `-f` / `--force` runs a full sign-in first —
`aws login`, `gh auth login`, `auth0 login` — for whichever provider was named,
or for all three when none was, and then reports the session that now exists.
`--logout` ends a session instead of checking it; asking for both at once is
refused. Those commands prompt or open a browser, so they inherit the terminal:
where there is a terminal and a required provider is not ready, mstage offers to
run the sign-in and re-checks the result rather than trusting the exit status.
Where there is none — CI — it reports and exits.

Where a person cannot answer a prompt, `signInWithClientCredentials` signs in as
an application instead. The Auth0 CLI supports both modes and behaves the same
afterwards, so `auth0 api …` works either way:

```js
import { signInWithClientCredentials } from 'mstage/auth'

signInWithClientCredentials('auth0', { domain, clientId, clientSecret })
```

Where those credentials come from is the caller's problem, not mstage's. On this
platform they live in the stage's secret store, which cannot be read before AWS
credentials exist — so that read has to follow a successful `login aws`, and the
machine login follows the read. The resulting session replaces whatever session
the CLI held for that tenant, which on a personal machine costs the operator
their interactive one.

`-f` must sit to the right of the `--`: npm owns `-f` as its own `--force`, and
takes it silently otherwise. mstage detects that and says so.

`resolveIdentity` passes the credential _provider_ through rather than a resolved
key triple, so short-lived sources keep refreshing. Child processes are the
exception — they get the resolved triple with every other AWS variable cleared.
On this platform that is load-bearing: `aws login` writes `login_session`, which
the AWS CLI and the JS SDK understand but the Go SDK behind SST and Pulumi does
not, so a Go tool started from such a shell finds nothing. Resolving in JS and
passing the result down is what lets `sst deploy` run at all.

| Value          | Precedence                                                                             |
| -------------- | -------------------------------------------------------------------------------------- |
| `stage`        | `--stage` › `MSTAGE_STAGE` › error                                                     |
| `app`          | `--app` › `MSTAGE_APP` › `mstage.config.json`                                          |
| `region`       | `--region` › the stage's declared region › `AWS_REGION` › `AWS_DEFAULT_REGION` › error |
| `role`         | `--role-arn` › `MSTAGE_AWS_ROLE_ARN` › the stage's declared role                       |
| `session name` | `--role-session-name` › `MSTAGE_AWS_ROLE_SESSION_NAME` › `mstage`                      |

An unresolvable region is an error rather than a silent `us-east-1`
(`aws.go:88`, `aws.go:108`). The one exception is `mstage login`, which has no
stage and where the region only picks an STS endpoint. The session name is only
used when a role is being assumed, and only names that session in CloudTrail.

## Tests

```bash
cd apps/infra && npm install
npm run test:mstage
npm run test:mdeploy
npm run typecheck --prefix mstage
```

These are not part of the repository-root `npm test`, because `apps/infra` is
not a root workspace and its dependencies are installed separately. Running them
in CI needs a step that installs `apps/infra` first; there is none yet, so
nothing runs these except a person.
