PHONY_TARGETS += test test\:unit\:guest test\:guest-perms test\:guest-artifacts test\:perf\:import-export _ensure-infra-deps test\:apps\:infra test\:apps\:infra-config test\:skill\:boxlite-diagrams

# Mirrors GitHub Actions strategy.fail-fast. Default false: aggregator
# targets run every sub-suite even if an earlier one fails, then exit
# non-zero with a summary of which suites failed. Set FAIL_FAST=true to
# abort at the first failing suite (original behavior).
FAIL_FAST ?= false
export FAIL_FAST

# Scope a run by test name: make <target> FILTER=<pattern>. Empty = full
# suite. Works for every test target. FILTER is mapped to each runner's
# native name selector, so match semantics are per-runner (nextest/ctest/
# go: regex, pytest -k / vitest -t: expression/substring).
export FILTER

# Advanced nextest-only filter expression. Use this for CI-specific exclusions
# that cannot be expressed as a simple positive FILTER pattern.
export NEXTEST_FILTER_EXPR

NEXTEST_FILTER   = $(if $(NEXTEST_FILTER_EXPR),-E '$(NEXTEST_FILTER_EXPR)',$(if $(FILTER),-E 'test(~$(FILTER))',))
NEXTEST_CLI_FILTER = $(if $(NEXTEST_FILTER_EXPR),-E '$(NEXTEST_FILTER_EXPR)',$(if $(FILTER),-E 'test(~$(FILTER))',-E 'not binary(stress_disk)'))
CARGOTEST_FILTER = $(if $(FILTER),$(FILTER),)
# The `--features rest` pass below is the only one that compiles the `rest`
# module at all — the two passes above build --no-default-features — so every
# rest-gated test lives or dies by this filter. It covers the whole module:
# scoped to one test module it silently skipped the other six.
REST_CARGOTEST_FILTER = $(if $(FILTER),$(FILTER),rest::)
PYTEST_FILTER    = $(if $(FILTER),-k '$(FILTER)',)
VITEST_FILTER    = $(if $(FILTER),-t '$(FILTER)',)
CTEST_FILTER     = $(if $(FILTER),-R '$(FILTER)',)
GOTEST_FILTER    = $(if $(FILTER),-run '$(FILTER)',)

CLI_INTEGRATION_TESTS = $(basename $(notdir $(filter-out src/cli/tests/stress_disk.rs,$(wildcard src/cli/tests/*.rs))))

# $(call run_suites,<space-separated make targets>)
# Runs each target via a recursive $(MAKE). With FAIL_FAST=false the loop
# continues past failures and exits non-zero at the end listing the
# failed targets; with FAIL_FAST=true it breaks on the first failure.
define run_suites
	@rc=0; failed=""; \
	for t in $(1); do \
		echo ""; \
		if ! $(MAKE) $$t; then \
			rc=1; failed="$$failed $$t"; \
			if [ "$(FAIL_FAST)" = "true" ]; then break; fi; \
		fi; \
	done; \
	echo ""; \
	if [ $$rc -ne 0 ]; then \
		echo "❌ Failed suites:$$failed"; \
		exit 1; \
	fi
endef

# Heavy shared setup (runtime build + Rust image cache) is built ONCE by the
# integration aggregators before they fan out; SETUP_DONE=1 tells the per-suite
# recursive sub-makes (and the dev:* prereqs) to skip rebuilding it. Running a
# leaf directly (SETUP_DONE unset) still builds its own prerequisites.
export SETUP_DONE

# $(call run_integration_suites,<space-separated make targets>)
# Like run_suites, but builds the shared runtime once up front (unless a parent
# already did, signalled by SETUP_DONE) and runs every suite with SETUP_DONE=1
# so no sub-make re-derives the phony runtime:debug / warm-cache prereqs.
define run_integration_suites
	@if [ -z "$(SETUP_DONE)" ]; then \
		echo "🔧 Preparing shared test runtime (once)..."; \
		$(MAKE) runtime:debug || exit 1; \
		if echo "$(1)" | grep -qE 'test:integration:(rust|core)'; then \
			$(MAKE) test:warm-cache:rust SETUP_DONE=1 || exit 1; \
		fi; \
	fi
	@rc=0; failed=""; \
	for t in $(1); do \
		echo ""; \
		if ! $(MAKE) $$t SETUP_DONE=1; then \
			rc=1; failed="$$failed $$t"; \
			if [ "$(FAIL_FAST)" = "true" ]; then break; fi; \
		fi; \
	done; \
	echo ""; \
	if [ $$rc -ne 0 ]; then \
		echo "❌ Failed suites:$$failed"; \
		exit 1; \
	fi
endef

# Default test target runs only changed components.
test:
	@$(MAKE) test:changed

test\:setup\:submodules:
	@bash $(SCRIPT_DIR)/test/test-setup-submodules.sh

# Pinned BoxLite diagram skill validator. This suite is deterministic: it uses
# local stand-ins for gh and Mermaid CLI while exercising the real parser,
# source, diff, and cross-view validation code from agent-tooling.
test\:skill\:boxlite-diagrams:
	@echo "🧪 Running BoxLite diagrams skill validator tests..."
	@./.agent-tooling/install.sh >/dev/null
	@hooks_path="$$(git config --get core.hooksPath)"; \
	tooling_root="$$(cd "$$hooks_path/.." && pwd)"; \
	python3 -m unittest discover -s "$$tooling_root/.agents/skills/boxlite-diagrams/tests" -v

# Smart test: only test components with changes, fall back to full matrix.
test\:changed:
ifeq ($(CHANGED_COMPONENTS),)
	@echo "📋 No changed components detected — skipping tests."
	@echo "   (Use 'make test:all' to run the full test matrix)"
else
	@echo "📋 Changed components: $(CHANGED_COMPONENTS)"
	$(call run_suites,$(addprefix test:changed:,$(sort $(CHANGED_COMPONENTS))))
	@echo ""
	@echo "✅ All changed-component tests passed"
endif

# Per-component test dispatch targets (map component tag → existing test targets).
#
# rust runs the reference-server suite first: its error table is keyed on the
# Display text of `BoxliteError` (src/shared/src/errors.rs, this component), so
# a new variant leaves the reference server answering the caller's mistake as a
# server fault. It costs milliseconds and fails before the long suites start.
test\:changed\:rust:
	@$(MAKE) test:unit:openapi
	@$(MAKE) test:unit:rust
	@$(MAKE) test:integration:rust

test\:changed\:cli:
	@$(MAKE) test:integration:cli

test\:changed\:ffi:
	@$(MAKE) test:unit:ffi

test\:changed\:python:
	@$(MAKE) test:all:python

test\:changed\:node:
	@$(MAKE) test:all:node

test\:changed\:c:
	@$(MAKE) test:unit:c
	@$(MAKE) test:integration:c

test\:changed\:go:
	@$(MAKE) test:unit:go

test\:changed\:apps:
	@$(MAKE) test:apps

# The Box API contract and the reference server that implements it.
test\:changed\:openapi:
	@$(MAKE) test:unit:openapi

# Workflow and composite-action changes. Runs the infra suite rather than the whole apps matrix:
# that suite is what asserts across .github (caller/callee permissions, Environment allowlists,
# the deploy step list, and the composite actions' own shell), and it finishes in seconds.
test\:changed\:ci:
	@$(MAKE) test:apps:infra

# Integration-only for changed components (used by E2E CI on PRs).
test\:integration\:changed:
ifeq ($(CHANGED_COMPONENTS),)
	@echo "📋 No changed components detected — skipping integration tests."
else
	@echo "📋 Running integration tests for changed components: $(CHANGED_COMPONENTS)"
	$(call run_integration_suites,$(strip \
		$(if $(filter rust,$(CHANGED_COMPONENTS)),test:integration:rust) \
		$(if $(filter cli,$(CHANGED_COMPONENTS)),test:integration:cli) \
		$(if $(filter python,$(CHANGED_COMPONENTS)),test:integration:python) \
		$(if $(filter node,$(CHANGED_COMPONENTS)),test:integration:node) \
		$(if $(filter c,$(CHANGED_COMPONENTS)),test:integration:c)))
	@echo ""
	@echo "✅ Changed-component integration tests passed"
endif

# Full matrix: all unit suites + all integration suites.
test\:all:
	@echo "📋 Running full test matrix (unit → integration)"
	$(call run_suites,test:unit test:integration test:apps)
	@echo ""
	@echo "✅ All tests passed (full matrix)"

# Unit matrix.
test\:unit:
	@echo "── Unit tests (core, sdk) ──"
	$(call run_suites,test:unit:core test:unit:sdk)
	@echo ""
	@echo "✅ Unit test matrix passed"

# Integration matrix.
test\:integration:
	@echo "── Integration tests (core, sdk) ──"
	$(call run_integration_suites,test:integration:core test:integration:sdk)
	@echo ""
	@echo "✅ Integration test matrix passed"

# Stress suites: intentionally excluded from the default integration matrix.
test\:stress:
	@echo "── Stress tests ──"
	$(call run_integration_suites,test:stress:disk)
	@echo ""
	@echo "✅ Stress test matrix passed"

# Core unit suites: Rust unit + FFI unit + gvproxy bridge unit.
test\:unit\:core:
	@echo "── Core unit suites (rust, openapi, ffi, gvproxy) ──"
	$(call run_suites,test:unit:rust test:unit:openapi test:unit:ffi test:unit:gvproxy)

# Core integration suites: Rust integration + CLI integration.
test\:integration\:core:
	@echo "── Core integration suites (rust, cli) ──"
	$(call run_integration_suites,test:integration:rust test:integration:cli)

# SDK unit suites: Python unit + Node unit + C unit + Go unit.
test\:unit\:sdk:
	@echo "── SDK unit suites (python, node, c, go) ──"
	$(call run_suites,test:unit:python test:unit:node test:unit:c test:unit:go)

# SDK integration suites: Python integration + Node integration + C SDK test suite.
test\:integration\:sdk:
	@echo "── SDK integration suites (python, node, c) ──"
	$(call run_integration_suites,test:integration:python test:integration:node test:integration:c)

# Rust unit tests (parallel via nextest, fallback to serial cargo test).
# --no-default-features disables gvproxy to avoid Go runtime link issues.
#
# Status accumulation: both crates always run (so a `-p boxlite-shared`
# regression isn't masked by a `-p boxlite` failure aborting first), but
# the recipe exits non-zero if EITHER crate failed. POSIX shell otherwise
# evaluates `cmd_a; cmd_b` as the rc of cmd_b, silently swallowing cmd_a.
test\:unit\:rust:
	@echo "🧪 Running Rust unit tests..."
	@rc=0; \
	if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --no-tests=warn -p boxlite --no-default-features --lib $(NEXTEST_FILTER) || rc=$$?; \
		cargo nextest run --no-tests=warn -p boxlite-shared --lib $(NEXTEST_FILTER) || rc=$$?; \
	else \
		cargo test -p boxlite --no-default-features --lib -- --test-threads=1 $(CARGOTEST_FILTER) || rc=$$?; \
		cargo test -p boxlite-shared --lib -- --test-threads=1 $(CARGOTEST_FILTER) || rc=$$?; \
	fi; \
	cargo test -p boxlite --no-default-features --features rest --lib -- --test-threads=1 $(REST_CARGOTEST_FILTER) || rc=$$?; \
	exit $$rc

# Guest crate unit tests. Linux-only (the crate does not build elsewhere) and
# excluded from test:unit:rust because the zygote suite forks real processes.
# nextest's process-per-test isolation is what makes the whole crate runnable:
# several suites assert on process-global state (raw fd numbers, waitpid), so
# they can interfere under a shared process. The cargo fallback has no such
# isolation, so it stays on the modules that are pure logic.
test\:unit\:guest:
	@if [ "$$(uname)" != "Linux" ]; then \
		echo "⏭️  Guest unit tests require Linux"; \
		exit 0; \
	fi; \
	echo "🧪 Running guest unit tests..."; \
	if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run --no-tests=fail -p boxlite-guest; \
	else \
		cargo test -p boxlite-guest --bins -- --test-threads=1 capabilit spec::tests sysctl::tests; \
	fi

# Keep ordinary ownership tests unprivileged. Only explicitly ignored tests
# whose names contain `privileged_` run as root, inside a private mount
# namespace so bind mounts cannot escape into the host namespace.
test\:guest-perms:
	@if [ "$$(uname)" != "Linux" ]; then \
		echo "⏭️  Guest ownership tests require Linux"; \
		exit 0; \
	fi; \
	echo "🧪 Running guest ownership tests..."; \
	cargo test -p boxlite-guest --bin boxlite-guest 'storage::perms::tests' -- \
		--test-threads=1 --skip privileged_ || exit $$?; \
	if ! command -v jq >/dev/null 2>&1; then \
		echo "❌ jq is required to locate the prebuilt guest test executable"; \
		exit 1; \
	fi; \
	if ! test_metadata=$$(mktemp "$${TMPDIR:-/tmp}/boxlite-guest-perms.XXXXXX"); then \
		echo "❌ Could not create temporary Cargo metadata file"; \
		exit 1; \
	fi; \
	trap 'rm -f -- "$$test_metadata"' EXIT; \
	if ! cargo test -p boxlite-guest --bin boxlite-guest --no-run \
		--message-format=json >"$$test_metadata"; then \
		echo "❌ Failed to build the boxlite-guest test executable"; \
		exit 1; \
	fi; \
	if ! test_binary=$$(jq --slurp --raw-output --exit-status \
		'[.[] | select(.reason == "compiler-artifact" and .profile.test == true and .target.name == "boxlite-guest" and .executable != null) | .executable] | last' \
		"$$test_metadata"); then \
		echo "❌ Could not parse the prebuilt boxlite-guest test executable"; \
		exit 1; \
	fi; \
	if [ -z "$$test_binary" ] || [ ! -x "$$test_binary" ]; then \
		echo "❌ Could not locate the prebuilt boxlite-guest test executable"; \
		exit 1; \
	fi; \
	echo "🔐 Running privileged guest ownership tests..."; \
	if [ "$$(id -u)" -eq 0 ]; then \
		unshare --mount --propagation private -- \
			"$$test_binary" 'storage::perms::tests::privileged_' \
			--ignored --test-threads=1; \
	elif command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then \
		sudo -n unshare --mount --propagation private -- \
			"$$test_binary" 'storage::perms::tests::privileged_' \
			--ignored --test-threads=1; \
	else \
		echo "❌ Privileged ownership tests require root or passwordless sudo"; \
		exit 1; \
	fi

# Build and qualify the standalone guest artifacts. Native x86_64 release also
# exercises verifier rejection paths; matching native Linux runs tool smoke tests.
test\:guest-artifacts: export _BOXLITE_GUEST_TARGET_ARG := $(value GUEST_TARGET)
test\:guest-artifacts: export _BOXLITE_PROFILE_ARG := $(value PROFILE)
test\:guest-artifacts:
	@target="$${_BOXLITE_GUEST_TARGET_ARG:-$$(bash "$$PWD/scripts/util.sh" --target)}"; \
	profile="$${_BOXLITE_PROFILE_ARG:-release}"; \
	bash "$$PWD/scripts/test/test-guest-artifacts.sh" --target "$$target" --profile "$$profile"

# Pre-warm Rust integration test image cache (internal helper, still callable).
test\:warm-cache\:rust: $(if $(SETUP_DONE),,runtime\:debug)
	@echo "🔥 Warming Rust integration test image cache..."
	@mkdir -p /tmp/boxlite-test
	@./target/debug/boxlite --home /tmp/boxlite-test \
		--registry docker.m.daocloud.io \
		--registry docker.xuanyuan.me \
		--registry docker.1ms.run \
		--registry docker.io \
		pull alpine:latest 2>/dev/null || \
		echo "  ⚠️ Pre-warm skipped (pull failed, tests will pull on-demand)"
	@echo "✅ Rust integration image cache ready"

# Rust integration tests (requires VM environment).
# FILTER works here and on every test target, e.g. make test:integration:rust FILTER=copy
test\:integration\:rust: $(if $(SETUP_DONE),,runtime\:debug test\:warm-cache\:rust)
	@echo "🧪 Running Rust integration tests (requires VM)..."
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run -p boxlite --features krun,gvproxy --test '*' --no-fail-fast --profile vm \
			$(NEXTEST_FILTER); \
	else \
		cargo test -p boxlite --features krun,gvproxy --test '*' --no-fail-fast -- --test-threads=1 --nocapture \
			$(CARGOTEST_FILTER); \
	fi

# Manual release-mode benchmark. Kept out of every aggregate and CI suite.
test\:perf\:import-export: runtime
	@echo "📊 Running manual 1 GiB import/export benchmark (release, serial)..."
	@echo "   Commit: $$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
	@echo "   Ensure at least 8 GiB of free disk space and no competing I/O workload."
	@cargo test --release -p boxlite --features krun,gvproxy \
		--test import_export_benchmark -- --ignored --test-threads=1 --nocapture

# BoxLite C SDK unit tests.
test\:unit\:ffi:
	@echo "🧪 Running BoxLite C SDK unit tests..."
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run -p boxlite-c $(NEXTEST_FILTER); \
	else \
		cargo test -p boxlite-c $(CARGOTEST_FILTER); \
	fi

# Go bridge unit tests for the embedded gvproxy library.
test\:unit\:gvproxy:
	@echo "🧪 Running gvproxy bridge unit tests..."
	@cd src/deps/libgvproxy-sys/gvproxy-bridge && go test ./... $(GOTEST_FILTER)

# OpenAPI reference-server unit tests: the error class the server answers with
# must be one openapi/box.openapi.yaml declares, and must match the triple
# `BoxliteError::http()` (src/shared/src/errors.rs) gives the same failure —
# the reference server is what src/boxlite/tests/rest_integration.rs points the
# Rust client at, so a class it gets wrong is a class every conformance run
# gets wrong.
#
# Discovery is restricted to the modules that import only the stdlib
# (openapi/reference-server/errors.py, like config.py). The rest of that
# directory needs python-dotenv, fastapi and pydantic, which no setup target
# here installs — discovering the whole directory would fail this suite on
# every checkout that does not intend to run the server.
test\:unit\:openapi:
	@echo "🧪 Running OpenAPI reference-server unit tests..."
	@python3 -m unittest discover -s openapi/reference-server/tests -p 'test_error*.py' -v

# CLI integration tests.
test\:integration\:cli: $(if $(SETUP_DONE),,runtime\:debug)
	@echo "🧪 Running CLI integration tests..."
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run -p boxlite-cli --tests --profile vm --no-fail-fast \
		$(NEXTEST_CLI_FILTER); \
	else \
		rc=0; \
		for test_name in $(CLI_INTEGRATION_TESTS); do \
			cargo test -p boxlite-cli --test $$test_name --no-fail-fast -- --test-threads=4 \
			$(CARGOTEST_FILTER) || rc=$$?; \
		done; \
		exit $$rc; \
	fi

test\:stress\:disk: $(if $(SETUP_DONE),,runtime\:debug)
	@echo "🧪 Running disk pressure stress tests..."
	@if command -v cargo-nextest >/dev/null 2>&1; then \
		cargo nextest run -p boxlite-cli --test stress_disk --profile vm --no-fail-fast \
		$(NEXTEST_FILTER); \
	else \
		cargo test -p boxlite-cli --test stress_disk --no-fail-fast -- --test-threads=1 \
		$(CARGOTEST_FILTER); \
	fi

# Python SDK unit tests.
test\:unit\:python: _ensure-python-deps
	@echo "🧪 Running Python SDK unit tests..."
	@. .venv/bin/activate && cd sdks/python && python -m pytest tests/ -v -m "not integration" $(PYTEST_FILTER)
	@echo "🧪 Running Python binding (Rust) unit tests..."
	@# --no-default-features drops pyo3's extension-module so the test binary can
	@# link libpython; see sdks/python/Cargo.toml.
	@cargo test -p boxlite-python --no-default-features --lib $(CARGOTEST_FILTER)

# Python SDK integration tests.
test\:integration\:python:
	@$(MAKE) dev:python
	@echo "🧪 Running Python SDK integration tests..."
	@BOXLITE_HOME=$$(mktemp -d /tmp/boxlite-test-python-XXXXXX) && \
	 trap "rm -rf $$BOXLITE_HOME" EXIT && \
	 . .venv/bin/activate && cd sdks/python && BOXLITE_HOME=$$BOXLITE_HOME python -m pytest tests/ -v -m "integration" $(PYTEST_FILTER)

# Python SDK full suite.
test\:all\:python:
	$(call run_suites,test:unit:python test:integration:python)

# Node.js SDK unit tests.
test\:unit\:node: _ensure-node-deps
	@echo "🧪 Running Node.js SDK unit tests..."
	@cd sdks/node && npm test -- $(VITEST_FILTER)
	@echo "🧪 Running Node.js binding (Rust) unit tests..."
	@cargo test -p boxlite-node --lib $(CARGOTEST_FILTER)

# Node.js SDK integration tests (requires VM environment).
test\:integration\:node:
	@$(MAKE) dev:node
	@echo "🧪 Running Node.js SDK integration tests (requires VM)..."
	@cd sdks/node && npm run test:integration -- $(VITEST_FILTER)

# Node.js SDK full suite.
test\:all\:node:
	$(call run_suites,test:unit:node test:integration:node)

# C SDK unit tests (no VM required).
test\:unit\:c:
	@echo "🧪 Running C SDK unit tests..."
	@$(MAKE) dev:c
	@mkdir -p sdks/c/tests/build
	@cd sdks/c/tests/build && cmake ..
	@cd sdks/c/tests/build && cmake --build . -j
	@cd sdks/c/tests/build && ctest --verbose --output-on-failure -L unit $(CTEST_FILTER)

# C SDK integration tests (requires VM environment).
test\:integration\:c:
	@echo "🧪 Running C SDK integration tests (requires VM)..."
	@$(MAKE) dev:c
	@mkdir -p sdks/c/tests/build
	@cd sdks/c/tests/build && cmake ..
	@cd sdks/c/tests/build && cmake --build . -j
	@cd sdks/c/tests/build && ctest --verbose --output-on-failure -L integration $(CTEST_FILTER)

# C SDK full suite.
test\:all\:c:
	$(call run_suites,test:unit:c test:integration:c)

# Go SDK unit tests.
test\:unit\:go:
	@echo "🧪 Running Go SDK unit tests..."
	@$(MAKE) dev:go
	@cd sdks/go && go test -tags boxlite_dev -v $(GOTEST_FILTER) ./...

# Go SDK archive round-trip integration test. Intentionally excluded from
# default test matrices and CI.
test\:integration\:go: dev\:go
	@echo "🧪 Running Go SDK archive integration test (requires VM)..."
	@cd sdks/go && go test -count=1 -tags=boxlite_dev,boxlite_integration -v $(GOTEST_FILTER) ./integration/archive

# Go SDK full suite.
test\:all\:go:
	@$(MAKE) test:unit:go

# apps/ workspace test matrix (all Nx projects). FILTER maps to Jest's
# --testNamePattern (per-runner semantics, like the other suites).
#
# Depends on dev:go so target/debug/libboxlite.a is built before any Nx Go
# project links against it. GOFLAGS=-tags=boxlite_dev (inherited by every
# `go test` Nx spawns) routes the cgo build through sdks/go/bridge_cgo_dev.go,
# which links target/debug/libboxlite.a + the in-repo sdks/c/include headers.
# Without the tag, `go test` uses bridge_cgo_prebuilt.go, which needs the
# downloaded sdks/go/{libboxlite.a,include} bundle (absent on dev checkouts)
# and fails #579. Same pattern as test:unit:go above.
_ensure-infra-deps:
	@if [ ! -d apps/infra/node_modules ]; then \
		echo "📦 Installing SST deployment dependencies..."; \
		cd apps/infra && npm ci --no-audit --no-fund; \
	fi

# Type-check first: the suite runs under `tsx --test`, which strips types without checking them, so
# a signature change that no longer compiles still leaves every test green. tsconfig.tooling.json
# is the subset that checks without `sst install`; test:apps:infra-config below covers the rest.
test\:apps\:infra: _ensure-infra-deps
	@echo "🧪 Type-checking infrastructure scripts..."
	@cd apps/infra && npm run --silent typecheck:tooling
	@echo "🧪 Running infrastructure script tests..."
	@cd apps/infra && npm test

test\:apps\:infra-config: _ensure-apps-deps _ensure-infra-deps
	@echo "🧪 Installing and type-checking the SST deployment config..."
	@cd apps/infra && IAM_PERMISSIONS_BOUNDARY_STAGE=ci npm run sst -- install --stage ci
	@cd apps/infra && npm run typecheck

test\:apps: _ensure-apps-deps dev\:go
	@echo "🧪 Running apps workspace test matrix..."
	@$(MAKE) test:apps:infra
	@cd apps && GOFLAGS=-tags=boxlite_dev yarn nx run-many --target=test --all --parallel=$$(getconf _NPROCESSORS_ONLN) $(if $(FILTER),-- --testNamePattern '$(FILTER)',)

test\:rest\:inventory: _ensure-apps-deps
	@cd apps && yarn node ../scripts/test/rest/inventory.mjs

test\:rest\:cli:
	@bash scripts/test/rest/run_cli_matrix.sh "$${AUTH:-oidc}" "$${SCOPE:-smoke}"

test\:rest\:e2e:
	@cd apps/e2e && BOXLITE_E2E_AUTH=$${AUTH:-api-key} python3 -m pytest cases/ -v $(PYTEST_FILTER)

test\:rest\:report:
	@python3 scripts/test/rest/report.py

# Installer-script smoke test: structural assertions on the rendered
# install.sh (atomic replace, integrity envelope, pinned-install trust
# tiers). Runs in a couple of seconds, no toolchain required beyond sh.
test\:install-script:
	@echo "🧪 Running installer script smoke test..."
	@bash scripts/release/test_install.sh

# ─── E2E: SDK → API → Runner → VM ───────────────────────────────────────────
test\:e2e\:setup:
	@apps/e2e/bootstrap.sh
	@python3 apps/e2e/fixture_setup.py

test\:e2e:
	@cd apps/e2e && python3 -m pytest cases/ -v

test\:e2e\:two-sided:
	@PR_REF=$${PR_REF:?must set PR_REF=<branch>} bash apps/e2e/two_sided.sh

# ─── Stress: deployed REST API ─────────────────────────────────────────────
test\:stress\:api-read:
	@command -v k6 >/dev/null 2>&1 || { echo "k6 is required. Install with: brew install k6"; exit 2; }
	@k6 run scripts/test/stress/api-read.k6.js

test\:stress\:api-read-local:
	@command -v k6 >/dev/null 2>&1 || { echo "k6 is required. Install with: brew install k6"; exit 2; }
	@BOXLITE_STRESS_START_RATE=$${BOXLITE_STRESS_START_RATE:-1} \
	BOXLITE_STRESS_RATE_1=$${BOXLITE_STRESS_RATE_1:-1} \
	BOXLITE_STRESS_RATE_2=$${BOXLITE_STRESS_RATE_2:-3} \
	BOXLITE_STRESS_RATE_3=$${BOXLITE_STRESS_RATE_3:-5} \
	BOXLITE_STRESS_STAGE_1=$${BOXLITE_STRESS_STAGE_1:-20s} \
	BOXLITE_STRESS_STAGE_2=$${BOXLITE_STRESS_STAGE_2:-20s} \
	BOXLITE_STRESS_STAGE_3=$${BOXLITE_STRESS_STAGE_3:-20s} \
	BOXLITE_STRESS_RAMP_DOWN=$${BOXLITE_STRESS_RAMP_DOWN:-5s} \
	BOXLITE_STRESS_PREALLOCATED_VUS=$${BOXLITE_STRESS_PREALLOCATED_VUS:-10} \
	BOXLITE_STRESS_MAX_VUS=$${BOXLITE_STRESS_MAX_VUS:-30} \
	BOXLITE_STRESS_P95_MS=$${BOXLITE_STRESS_P95_MS:-2500} \
	BOXLITE_STRESS_P99_MS=$${BOXLITE_STRESS_P99_MS:-4000} \
	k6 run --summary-trend-stats 'avg,min,med,p(90),p(95),p(99),max' scripts/test/stress/api-read.k6.js

test\:stress\:api-create-box:
	@command -v k6 >/dev/null 2>&1 || { echo "k6 is required. Install with: brew install k6"; exit 2; }
	@k6 run --summary-trend-stats 'avg,min,med,p(90),p(95),p(99),max' scripts/test/stress/api-create-box.k6.js

test\:stress\:api-create-box-local:
	@command -v k6 >/dev/null 2>&1 || { echo "k6 is required. Install with: brew install k6"; exit 2; }
	@BOXLITE_STRESS_START_RATE=$${BOXLITE_STRESS_START_RATE:-0.02} \
	BOXLITE_STRESS_RATE_1=$${BOXLITE_STRESS_RATE_1:-0.05} \
	BOXLITE_STRESS_RATE_2=$${BOXLITE_STRESS_RATE_2:-0.1} \
	BOXLITE_STRESS_RATE_3=$${BOXLITE_STRESS_RATE_3:-0.1} \
	BOXLITE_STRESS_STAGE_1=$${BOXLITE_STRESS_STAGE_1:-30s} \
	BOXLITE_STRESS_STAGE_2=$${BOXLITE_STRESS_STAGE_2:-1m} \
	BOXLITE_STRESS_STAGE_3=$${BOXLITE_STRESS_STAGE_3:-1m} \
	BOXLITE_STRESS_RAMP_DOWN=$${BOXLITE_STRESS_RAMP_DOWN:-20s} \
	BOXLITE_STRESS_PREALLOCATED_VUS=$${BOXLITE_STRESS_PREALLOCATED_VUS:-2} \
	BOXLITE_STRESS_MAX_VUS=$${BOXLITE_STRESS_MAX_VUS:-5} \
	BOXLITE_STRESS_P95_MS=$${BOXLITE_STRESS_P95_MS:-45000} \
	BOXLITE_STRESS_P99_MS=$${BOXLITE_STRESS_P99_MS:-60000} \
	k6 run --summary-trend-stats 'avg,min,med,p(90),p(95),p(99),max' scripts/test/stress/api-create-box.k6.js

test\:stress\:api-vm-lifecycle:
	@command -v k6 >/dev/null 2>&1 || { echo "k6 is required. Install with: brew install k6"; exit 2; }
	@k6 run --summary-trend-stats 'avg,min,med,p(90),p(95),p(99),max' scripts/test/stress/api-vm-lifecycle.k6.js

test\:stress\:api-vm-lifecycle-local:
	@command -v k6 >/dev/null 2>&1 || { echo "k6 is required. Install with: brew install k6"; exit 2; }
	@BOXLITE_STRESS_RUNNER_CPUS=$${BOXLITE_STRESS_RUNNER_CPUS:-8} \
	BOXLITE_STRESS_VM_LIMIT=$${BOXLITE_STRESS_VM_LIMIT:-2} \
	BOXLITE_STRESS_VUS=$${BOXLITE_STRESS_VUS:-2} \
	BOXLITE_STRESS_DURATION=$${BOXLITE_STRESS_DURATION:-1m} \
	BOXLITE_STRESS_HOLD_SECONDS=$${BOXLITE_STRESS_HOLD_SECONDS:-20} \
	BOXLITE_STRESS_GRACEFUL_STOP=$${BOXLITE_STRESS_GRACEFUL_STOP:-2m} \
	BOXLITE_STRESS_P95_MS=$${BOXLITE_STRESS_P95_MS:-60000} \
	BOXLITE_STRESS_P99_MS=$${BOXLITE_STRESS_P99_MS:-90000} \
	k6 run --summary-trend-stats 'avg,min,med,p(90),p(95),p(99),max' scripts/test/stress/api-vm-lifecycle.k6.js
