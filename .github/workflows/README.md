# CI/CD workflows

Every GitHub Actions workflow in the repository: the pull-request checks, the SDK build and
publish chain, the cloud deploy path, the three end-to-end suites, and the box images.

Shared step bundles live one directory over, in [`.github/actions/`](../actions) — GitHub does not
support subdirectories under `.github/workflows/`, so composite actions are where reuse goes.

## How they fit together

```text
PULL REQUEST / PUSH                     lint · test · codeql · api-client-drift
                                        e2e-stack · e2e-local · build-box-images

BUILD CHAIN (workflow_run)              warm-caches ──▶ build-runtime
                                        build-c ──▶ build-go
                                                └──▶ build-runner-binary

RELEASE (release event)                 build-runtime · build-c · build-node · build-wheels
                                        apps/box-images/v* tag ──▶ release-box-images

DEPLOY (manual dispatch)                deploy-infra ─┬─▶ build-apps-api-image
                                                      ├─▶ build-c ──▶ build-runner-binary
                                                      └─▶ e2e-cloud
                                        deploy-release   (no builds; consumes published artifacts)

CONFIG                                  config ◀── every build workflow, lint, test, warm-caches
```

## Workflows

**Callable** marks a workflow another one can invoke with `uses:`. `config.yml` is the only one that
is *exclusively* callable; the other four can also be dispatched on their own.

| Workflow | Triggers | Callable | Purpose |
| --- | --- | --- | --- |
| `config.yml` | `workflow_call` | call-only | Single source of the platform matrix and language versions |
| `lint.yml` | push, PR, merge_group | — | Format and lint per language, plus the infra suite. `Lint (conclusion)` is the required check |
| `test.yml` | push, PR, merge_group | — | Unit tests for every SDK. No VM tests — hosted runners have no nested virtualization |
| `codeql.yml` | push, PR, dispatch, weekly | — | CodeQL advanced setup, so fork PRs are scanned |
| `api-client-drift.yml` | PR | — | Fails if the committed generated clients no longer match their specs |
| `warm-caches.yml` | push, weekly, dispatch | — | Populates the sccache the other Rust builds read |
| `build-runtime.yml` | `workflow_run`, release, dispatch | — | Core runtime and CLI; publishes crates |
| `build-c.yml` | release, dispatch, `workflow_call` | yes | C SDK archives |
| `build-go.yml` | `workflow_run`, dispatch | — | Tests the Go SDK and tags its module |
| `build-node.yml` | release, dispatch | — | Node.js SDK, napi-rs addon and platform packages |
| `build-wheels.yml` | release, dispatch | — | Python wheels via cibuildwheel |
| `build-runner-binary.yml` | `workflow_run`, dispatch, `workflow_call` | yes | Linux amd64 runner binary |
| `build-apps-api-image.yml` | dispatch, `workflow_call` | yes | The `apps/api` image: build a commit, build a release, or promote one between stages |
| `deploy-infra.yml` | dispatch | — | Builds and deploys one commit to a stage. The normal deploy path |
| `deploy-release.yml` | dispatch | — | Deploys already-published artifacts for one `X.Y.Z`. Compiles nothing |
| `e2e-cloud.yml` | dispatch, `workflow_call` | yes | End-to-end against a deployed stage. Run by `deploy-infra` after it applies |
| `e2e-local.yml` | push, `pull_request_target`, dispatch | — | VM-based tests on a self-hosted EC2 runner. Needs `/dev/kvm`; PRs need the `e2e-local` label |
| `e2e-stack.yml` | push, PR, dispatch | — | SDK → API → runner → VM on a nested-KVM runner |
| `build-box-images.yml` | PR, push, dispatch | — | Builds every box image flavor for both arches without publishing |
| `release-box-images.yml` | `apps/box-images/v*` tag, dispatch | — | The only workflow that writes to GHCR |

Longer treatments live with their subject rather than here: [E2E local
runbook](../../docs/ci/e2e-local.md), [deployment](../../apps/infra/docs/deployment.md).

## Composite actions

In [`.github/actions/`](../actions). Each replaces a step bundle that was previously copied into
every consumer.

| Action | Sites | Used by |
| --- | --- | --- |
| `setup-rust` | 13 | build-c, build-node, build-runtime ×2, build-wheels, lint ×3, test ×4, warm-caches |
| `sccache` | 9 | build-c, build-node, build-runtime, build-wheels, lint ×2, test ×2, warm-caches |
| `build-guest` | 5 | build-c, build-node, build-runtime, build-wheels, warm-caches |
| `upload-to-release` | 5 | build-c, build-node, build-runner-binary, build-runtime, build-wheels |
| `run-in-manylinux` | 4 | build-c, build-node, build-runtime, warm-caches |
| `setup-go` | 4 | build-go, build-runner-binary, lint, test |
| `setup-python` | 3 | build-wheels, lint, test |
| `setup-buildx` | 2 | build-box-images, release-box-images |

Two ordering rules, stated in each action's own header: `sccache` runs after `setup-rust`, and
`build-guest` and `run-in-manylinux` run after both. They are separate actions rather than one
because sccache is job-scoped — `run-in-manylinux` mounts the sccache binary and reads the
variables `sccache` exported, long after `build-guest` is done with them.

A `uses: ./...` action is resolved from the **checked-out tree**, not from the ref that defines the
workflow. Jobs that check out a caller-selected commit — `build-c` and `build-runner-binary`, when
`deploy-infra` drives them — therefore need `.github/actions/` to exist in *that* commit. A commit
predating these directories fails with `Can't find 'action.yml'`; select a newer one.

## sccache

Rust compilation is cached with [sccache](https://github.com/mozilla-actions/sccache-action) over
the GitHub Actions cache API **in the jobs that invoke `./.github/actions/sccache`** — not in every
job that compiles Rust. `test.yml`'s `rust` and `guest_artifacts` jobs set up the toolchain without it,
so they compile uncached. The action owns the whole configuration; a caller only invokes it.

- Caches individual compilation units by content hash, so it works on the host and inside the
  Docker and cibuildwheel manylinux containers alike.
- Pre-warmed by `warm-caches.yml` on push to main; `build-runtime.yml` chains off it via
  `workflow_run` so the cache is hot.
- **`RUSTC_WRAPPER=sccache` and `SCCACHE_GHA_ENABLED` are set by the action.** The upstream
  `sccache-action` installs the binary and exports the cache credentials but sets neither, so a job
  that only installed it compiled uncached while still looking healthy. Setting them is what makes
  the cache take effect.
- `CARGO_INCREMENTAL=0` is set by the action too, unconditionally — sccache cannot cache
  incremental compilation, so it is a precondition rather than a caller's choice.
- `SCCACHE_BASEDIRS` strips the workspace prefix from cache keys, `$GITHUB_WORKSPACE` on the host
  and `/work` inside the container, so the two sides can share an entry instead of keying the same
  crate twice by absolute path.
- The sccache version is pinned in the action rather than floating on `latest`, which is what an
  omitted `version` means.
- Degrades rather than fails, **on the host**: `RUSTC_WRAPPER` is set only once the server is
  actually serving, so both a missing binary and a server that will not start leave it unset —
  pointing cargo at an sccache that cannot answer would turn a tolerated cache failure into a hard
  build failure. A tolerant job additionally gets `SCCACHE_IGNORE_SERVER_IO_ERROR`, which narrowly
  turns a failure to read the compile response from the server into a local compile rather than a
  failed build. Diagnostics land in `$RUNNER_TEMP/sccache-error.log`, and the action's post step
  prints hit/miss stats.
- **The containers decide separately, and depend on the host having sccache at all.**
  `run-in-manylinux` builds its whole `-e` list — the bind-mount, `RUSTC_WRAPPER`,
  `SCCACHE_GHA_ENABLED`, the basedir — inside `if command -v sccache` evaluated *on the host*, so a
  host with no binary caches nowhere. Given a binary, the container runs its own server and its
  prologue drops `RUSTC_WRAPPER` only if the mount did not arrive: it degrades on a missing binary,
  not on a failed startup, and `SCCACHE_IGNORE_SERVER_IO_ERROR` is not forwarded. cibuildwheel's
  container is the one that installs its own sccache (`sdks/python/pyproject.toml`), but it wraps
  cargo through the `RUSTC_WRAPPER` its `environment-pass` inherits from the host.
- `warm-caches.yml` passes `tolerate-failure: 'false'` and is the deliberate exception: populating
  the cache is its entire purpose, so every one of those paths fails the job rather than warning.

## CodeQL

`codeql.yml` uses CodeQL **advanced** setup rather than default setup, because default setup does
not analyze pull requests from forks — which makes the `code_scanning` ruleset rule ("Require code
scanning results") permanently block fork PRs. Advanced setup runs on `pull_request`, so fork PRs
in this public repo are scanned and the gate is satisfiable without an admin bypass.

The `analyze` job is a matrix over `actions`, `c-cpp`, `go`, `javascript-typescript`, `python` and
`rust`. All use `build-mode: none` (source only, no compile) except `go`, whose extractor has to
observe a real build and therefore uses `autobuild`.

## Do not rename these

Three workflows chain off another's **display name**, not its filename. Changing a `name:` below
silently stops the chain — no error, the downstream workflow simply never fires.

| `name:` | Depended on by |
| --- | --- |
| `Build C SDK` | `build-go.yml`, `build-runner-binary.yml` |
| `Warm Caches` | `build-runtime.yml` |

## Adding a stage

`stage` inputs are allowlists rather than free text, so a required-reviewers Environment cannot be
targeted by an unbootstrapped or misspelled name. Each list is independent — it names the stages
*that* path is meant to reach. Today `deploy-infra.yml` lists `dev`, while `deploy-release.yml` and
`build-apps-api-image.yml` (`stage` and `source_stage`) list `dev` and `prod`. Bootstrapping a
stage means adding it to whichever lists should reach it.

Each stage also needs its GitHub Environment to exist under exactly the stage name — the deploy
role's trust policy pins `repo:<owner>/<repo>:environment:<stage>` — and that is where required
reviewers are enforced.

## Deploy configuration

Per stage, on the GitHub side:

- **Environment variables** `AWS_ACCOUNT_ID` and `AWS_REGION`. Neither can live in the stage's SST
  secret store, because `configure-aws-credentials` reads them before any AWS credentials exist.
  - `AWS_ACCOUNT_ID` is **required**. The workflows compose
    `arn:aws:iam::<id>:role/boxlite-<stage>-github-deploy` from it; only the account id is unknown,
    since the role name follows from the stage.
  - `AWS_REGION` is **optional**, and only for a stage outside the default. The workflows fall back
    to `DEFAULT_AWS_REGION`, pinned to the code by a test.
- **Environment secrets** `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_DEFAULT_ACCOUNT_ID`. These cannot
  move to the SST secret store either: reading that store initializes the Cloudflare provider, so a
  token kept there would be needed in order to read itself.

Everything else for a stage lives in its SST secret store, seeded by `npm run bootstrap` and read
by `apps/infra/deployment/sst.ts`. `npm run bootstrap` also reconciles the scoped role, permissions
boundary, immutable API ECR repository and private runner artifact bucket, from the documents in
`apps/infra/bootstrap/aws/`.

## Publishing secrets

Repository secrets, in Settings → Secrets and variables → Actions:

- `CARGO_REGISTRY_TOKEN` — crates.io, for the Rust crates
- `PYPI_API_TOKEN` — PyPI, for the wheels
- `NPM_TOKEN` — npm, for the Node packages
- `GH_APP_PRIVATE_KEY` — GitHub App key that registers the self-hosted E2E runner
