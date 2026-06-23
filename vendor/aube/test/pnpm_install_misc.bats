#!/usr/bin/env bats
#
# Ported from pnpm/test/install/misc.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Note: pnpm uses `install <pkg>` for both "install everything" and "add a
# new dep". aube splits these — `aube install` only re-installs declared
# deps, and `aube add <pkg>` adds a new one. Tests that pass a package to
# `pnpm install` translate to `aube add` here.

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube add -E -D: combines --save-exact and --save-dev" {
	# Ported from pnpm/test/install/misc.ts:124 ('install --save-exact')
	# is-positive substituted with is-odd (already in test/registry/storage/).
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-save-exact-dev",
  "version": "0.0.0"
}
JSON

	run aube add -E -D is-odd@3.0.1
	assert_success
	assert_file_exists node_modules/is-odd/index.js

	run cat package.json
	assert_output --partial '"devDependencies"'
	assert_output --partial '"is-odd": "3.0.1"'
	refute_output --partial '"is-odd": "^'
	refute_output --partial '"is-odd": "~'
	# is-odd should land in devDependencies, not dependencies.
	refute_output --partial '"dependencies"'
}

@test "aube --use-stderr add: writes everything to stderr, stdout stays empty" {
	# Ported from pnpm/test/install/misc.ts:73 ('write to stderr when
	# --use-stderr is used'). is-positive substituted with is-odd.
	# pnpm's `install <pkg>` ≈ aube `add <pkg>`.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-use-stderr",
  "version": "0.0.0"
}
JSON

	run --separate-stderr aube --use-stderr add is-odd
	assert_success
	assert [ -z "$output" ]
	# `assert` can't wrap `[[ ... ]]` (bash keyword, not a command), so use grep.
	assert grep -qF "is-odd" <<<"$stderr"
}

@test "aube add: lockfile=false in pnpm-workspace.yaml suppresses aube-lock.yaml" {
	# Ported from pnpm/test/install/misc.ts:83 ('install with lockfile being
	# false in pnpm-workspace.yaml'). is-positive substituted with is-odd.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-lockfile-false",
  "version": "0.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
lockfile: false
YAML

	run aube add is-odd
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	assert_file_not_exists aube-lock.yaml
}

@test "aube install --lockfile-dir: writes the lockfile to a parent dir with a relative importer key" {
	# Ported from pnpm/test/install/misc.ts:112 ('install with external
	# lockfile directory'). pnpm `install <pkg> --lockfile-dir ../`
	# becomes aube `install --lockfile-dir ..` with the dep already
	# declared in package.json, since aube's flag is install-only and
	# `aube install` doesn't take a package argument. is-positive
	# substituted with is-odd. Implementation landed in #431.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "pnpm-misc-lockfile-dir",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cd project || return
	run aube install --lockfile-dir .. --no-frozen-lockfile
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	# Lockfile must land in the parent dir, not next to package.json.
	assert_file_exists ../aube-lock.yaml
	assert_file_not_exists aube-lock.yaml
	# Importer key in the lockfile is the project's path relative to
	# the lockfile dir — `project` here, not `.` (which would mean
	# the parent dir is itself the project).
	run grep -E '^[[:space:]]+project:$' ../aube-lock.yaml
	assert_success
}

@test "aube install --prefix: runs install in the named subdirectory" {
	# Ported from pnpm/test/install/misc.ts:97 ('install from any location
	# via the --prefix flag'). rimraf substituted with is-odd; we don't
	# assert on .bin/is-odd because is-odd doesn't ship a bin.
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "pnpm-misc-prefix",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	# Stay in the parent dir; --prefix points at the project subdir.
	run aube install --prefix project
	assert_success
	assert_file_exists project/node_modules/is-odd/index.js
}

@test "aube add: saves the dependency spec verbatim (no rewriting tilde to caret)" {
	# Ported from pnpm/test/install/misc.ts:150 ('install save new dep with
	# the specified spec'). is-positive@~3.1.0 substituted with is-odd@~3.0.0.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-spec-verbatim",
  "version": "0.0.0"
}
JSON

	run aube add is-odd@~3.0.0
	assert_success

	run cat package.json
	assert_output --partial '"is-odd": "~3.0.0"'
	refute_output --partial '"is-odd": "^'
}

@test "aube install: bin files from deps are on PATH for the root postinstall script" {
	# Ported from pnpm/test/install/misc.ts:36 ('bin files are found by
	# lifecycle scripts'). Uses the @pnpm.e2e/hello-world-js-bin fixture
	# now available via test/registry/storage/.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-bin-in-lifecycle",
  "version": "1.0.0",
  "dependencies": { "@pnpm.e2e/hello-world-js-bin": "*" },
  "scripts": { "postinstall": "hello-world-js-bin" }
}
JSON

	run aube install
	assert_success
	assert_output --partial "Hello world!"
}

@test "aube run: a script can invoke a bin from an installed dep" {
	# Ported from pnpm/test/install/misc.ts:219 ('run js bin file').
	# pnpm runs `npm test`; we use `aube run test` to keep the assertion
	# purely about aube's PATH wiring for run-scripts.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-run-js-bin",
  "version": "1.0.0",
  "scripts": { "test": "hello-world-js-bin" }
}
JSON

	run aube add @pnpm.e2e/hello-world-js-bin
	assert_success

	run aube run test
	assert_success
	assert_output --partial "Hello world!"
}

@test "aube add: a top-level bin can require a sibling top-level package" {
	# Ported from pnpm/test/install/misc.ts:190 ('top-level packages should
	# find the plugins they use'). Uses the @pnpm.e2e/pkg-that-uses-plugins
	# and @pnpm.e2e/plugin-example fixtures from test/registry/storage/.
	# pnpm runs `npm test`; we use `aube run test` to keep the assertion
	# purely about aube's resolution wiring for top-level deps.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-top-level-plugins",
  "version": "1.0.0",
  "scripts": { "test": "pkg-that-uses-plugins" }
}
JSON

	run aube add @pnpm.e2e/pkg-that-uses-plugins @pnpm.e2e/plugin-example
	assert_success

	run aube run test
	assert_success
	assert_output --partial "My plugin is @pnpm.e2e/plugin-example"
}

@test "aube add: a top-level dep's bin can require its own (non-top-level) dep" {
	# Ported from pnpm/test/install/misc.ts:204 ('not top-level packages
	# should find the plugins they use'). pnpm uses `standard@8.6.0` which
	# pulls in ~170 transitive deps; we substitute a minimal fixture
	# (aube-test-bin-uses-dep) whose bin requires @pnpm.e2e/dep-of-pkg-with-1-dep,
	# its declared regular dep that is NOT a top-level dep of the test
	# project. This exercises the same property: a top-level dep's bin
	# can resolve its own non-top-level deps via Node's parent-`node_modules`
	# walk under aube's isolated layout.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-not-top-level-plugins",
  "version": "1.0.0",
  "scripts": { "test": "aube-bin-uses-dep" }
}
JSON

	run aube add aube-test-bin-uses-dep
	assert_success

	run aube run test
	assert_success
	assert_output --partial "Loaded inner dep: @pnpm.e2e/dep-of-pkg-with-1-dep"
}

@test "aube add: creates package.json if there is none" {
	# Ported from pnpm/test/install/misc.ts:233 ('create a package.json
	# if there is none'). pnpm `install <pkg>` ≈ aube `add <pkg>`.
	# is-positive substituted with is-odd.

	# Deliberately no package.json in cwd. _common_setup parks us in a
	# fresh tmp dir with HOME isolated, so the find_project_root walk
	# can't escape into the user's real home and find a package.json
	# higher up.
	run aube add is-odd@3.0.1
	assert_success
	assert_file_exists package.json
	assert_file_exists node_modules/is-odd/index.js

	run cat package.json
	assert_output --partial '"is-odd"'
	assert_output --partial '"3.0.1"'
}

@test "aube add: fails when no package name is provided" {
	# Ported from pnpm/test/install/misc.ts:245 ('pnpm add should fail
	# if no package name was provided'). Asserts exit code + error text;
	# the wording is deliberately generic ('packages') so a future
	# rephrasing won't break the test.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-add-no-name",
  "version": "1.0.0"
}
JSON

	run aube add
	assert_failure
	assert_output --partial "no packages specified"
}

@test "aube add: a tarball with case-only filename collisions installs cleanly" {
	# Ported from pnpm/test/install/misc.ts:163 ('don't fail on case
	# insensitive filesystems when package has 2 files with same name').
	# pnpm's version asserts on its StoreIndex internals to confirm both
	# Foo.js and foo.js are tracked — that's pnpm-specific. We just assert
	# that the install succeeds and the package appears under node_modules,
	# which is the user-visible parity guarantee. The store-side
	# case-collision handling is an aube-internal CAS concern.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-case-conflict",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm.e2e/with-same-file-in-different-cases
	assert_success
	assert_dir_exists 'node_modules/@pnpm.e2e/with-same-file-in-different-cases'
	assert_file_exists 'node_modules/@pnpm.e2e/with-same-file-in-different-cases/package.json'
}

@test "aube install --lockfile-only: terminates on circular peer dependencies" {
	# Ported from pnpm/test/install/misc.ts:556 ('do not hang on circular
	# peer dependencies', covers pnpm/pnpm#8720). pnpm's fixture is a
	# 100+-package real-world workspace; we use the minimal shape that
	# reproduces the cycle (two workspace packages peer-depending on each
	# other) to keep the test hermetic. The regression guard is the
	# resolver actually terminating — bounded by the fixed-point loop in
	# aube-resolver/src/peer_context.rs (max 16 iterations).
	mkdir -p packages/a packages/b
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "packages/*"
YAML
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-circular-peers",
  "version": "1.0.0",
  "private": true
}
JSON
	cat >packages/a/package.json <<'JSON'
{
  "name": "circular-a",
  "version": "1.0.0",
  "dependencies": { "circular-b": "workspace:*" },
  "peerDependencies": { "circular-b": "workspace:*" }
}
JSON
	cat >packages/b/package.json <<'JSON'
{
  "name": "circular-b",
  "version": "1.0.0",
  "dependencies": { "circular-a": "workspace:*" },
  "peerDependencies": { "circular-a": "workspace:*" }
}
JSON

	# Hard 60s ceiling — if the resolver regresses into a hang, fail fast
	# instead of stalling the entire bats run. `timeout` is GNU-coreutils
	# (Linux/CI); macOS ships it only via `brew install coreutils` as
	# `gtimeout`. Probe for both, fall back to running uncovered if
	# neither is on PATH (the in-resolver 16-iteration bound is still a
	# hard guarantee).
	local timeout_cmd=""
	if command -v timeout >/dev/null 2>&1; then
		timeout_cmd="timeout 60"
	elif command -v gtimeout >/dev/null 2>&1; then
		timeout_cmd="gtimeout 60"
	fi
	# shellcheck disable=SC2086 # intentional word-split: empty -> no wrapper
	run $timeout_cmd aube install --lockfile-only
	assert_success
	assert_file_exists aube-lock.yaml
}

# Trust-policy block (pnpm misc.ts:578-643). pnpm's `--trust-policy=…`
# CLI flag has no aube counterpart; aube reads `trustPolicy` from
# `.npmrc` / `pnpm-workspace.yaml` / env (`AUBE_TRUST_POLICY`), so each
# port writes a small `.npmrc` instead of passing a flag. Fixtures:
# `@pnpm/e2e.test-provenance` mirrored at versions 0.0.0, 0.0.4 (with
# SLSA provenance + GitHub trustedPublisher), 0.0.5 (no evidence — the
# downgrade). `@pnpm.e2e/has-untrusted-optional-dep@1.0.0` already in
# the registry, optionally depends on `@pnpm/e2e.test-provenance@0.0.5`.

@test "trustPolicy=no-downgrade: install fails when picked version drops trust evidence" {
	# Ported from pnpm/test/install/misc.ts:578.
	# pnpm: --trust-policy=no-downgrade. aube: write to .npmrc.
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
trust-policy=no-downgrade
EOF
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-trust-fail",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm/e2e.test-provenance@0.0.5
	assert_failure
	assert_output --partial "trust downgrade for @pnpm/e2e.test-provenance@0.0.5"
	assert_file_not_exists node_modules/@pnpm/e2e.test-provenance/package.json
}

@test "trustPolicy=off: install succeeds even on a downgraded version" {
	# Ported from pnpm/test/install/misc.ts:589.
	# Aube's default trustPolicy is no-downgrade; the test must explicitly
	# turn it off to mirror pnpm's --trust-policy=off.
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
trust-policy=off
EOF
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-trust-off",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm/e2e.test-provenance@0.0.5
	assert_success
	assert_file_exists node_modules/@pnpm/e2e.test-provenance/package.json
}

@test "trustPolicyExclude with name@version: install succeeds for the listed version" {
	# Ported from pnpm/test/install/misc.ts:600.
	# pnpm: --trust-policy-exclude=@pnpm/e2e.test-provenance@0.0.5
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
trust-policy=no-downgrade
trust-policy-exclude=@pnpm/e2e.test-provenance@0.0.5
EOF
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-trust-exclude-version",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm/e2e.test-provenance@0.0.5
	assert_success
	assert_file_exists node_modules/@pnpm/e2e.test-provenance/package.json
}

@test "trustPolicyExclude with bare name: install succeeds for any version of that package" {
	# Ported from pnpm/test/install/misc.ts:612.
	# pnpm: --trust-policy-exclude=@pnpm/e2e.test-provenance (no version).
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
trust-policy=no-downgrade
trust-policy-exclude=@pnpm/e2e.test-provenance
EOF
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-trust-exclude-name",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm/e2e.test-provenance@0.0.5
	assert_success
	assert_file_exists node_modules/@pnpm/e2e.test-provenance/package.json
}

@test "trustPolicy=no-downgrade: install fails when an optional dep's trust evidence is downgraded" {
	# Ported from pnpm/test/install/misc.ts:624. The hard-fail behavior
	# is intentional even for optional deps — a supply-chain regression
	# in an optional package is still a supply-chain regression.
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
trust-policy=no-downgrade
EOF
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-trust-optional-fail",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm.e2e/has-untrusted-optional-dep@1.0.0
	assert_failure
	assert_output --partial "trust downgrade for @pnpm/e2e.test-provenance@0.0.5"
}

@test "trustPolicyIgnoreAfter: install succeeds when picked version is older than the cutoff" {
	# Ported from pnpm/test/install/misc.ts:635.
	# pnpm: --trust-policy-ignore-after=1440 (skip check for versions
	# published more than 1 day ago). The mirrored 0.0.5 was published
	# 2025-11-09, so the cutoff exempts it on any recent test run.
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
trust-policy=no-downgrade
trust-policy-ignore-after=1440
EOF
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-trust-ignore-after",
  "version": "1.0.0"
}
JSON

	run aube add @pnpm/e2e.test-provenance@0.0.5
	assert_success
	assert_file_exists node_modules/@pnpm/e2e.test-provenance/package.json
}

@test "strict-peer-dependencies: peer-deps warning renders without crashing the resolver" {
	# Ported from pnpm/test/install/misc.ts:541 ('do not fail to render
	# peer dependencies warning, when cache was hit during peer
	# resolution', covers pnpm/pnpm#8538). pnpm asserts status=0 + the
	# warning string in stdout — pnpm warns by default. aube is silent
	# by default (matching bun/npm/yarn) and `strict-peer-dependencies=
	# true` is the escape hatch that surfaces the same diagnostic. Aube
	# routes the diagnostic through a hard-fail, so this port asserts
	# `assert_failure` + the warning string instead of pnpm's
	# warn-and-succeed shape. The regression guard — that the warning
	# renderer doesn't crash when peers are missing — is preserved
	# either way. @udecode/plate-* substituted with the mirrored
	# `@pnpm.e2e/abc-parent-with-missing-peers` (depends on `abc`,
	# whose peer-a/peer-b/peer-c are unsatisfied).
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
auto-install-peers=false
strict-peer-dependencies=true
EOF
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-peer-deps-warning",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/abc-parent-with-missing-peers": "1.0.0"
  }
}
JSON

	run aube install
	assert_failure
	assert_output --partial "Issues with peer dependencies found"
}

@test "aube add --fetch-timeout=1 --fetch-retries=0: fails with a timeout error" {
	# Ported from pnpm/test/install/misc.ts:508 ('installation fails with
	# a timeout error'). typescript@2.4.2 substituted with is-odd (in
	# the local Verdaccio fixture) — package choice doesn't matter, the
	# packument fetch is what trips the timeout.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-fetch-timeout",
  "version": "0.0.0"
}
JSON

	run aube --fetch-timeout=1 --fetch-retries=0 add is-odd@3.0.1
	assert_failure
	# Pin the failure mode to "registry fetch aborted" so the test is
	# falsifiable against regressions that fail for the wrong reason —
	# e.g. a clap parse error on the new flags, a missing fixture, or
	# `aube add` bailing out before it reaches the registry. Depending on
	# which request hits the 1 ms deadline first, the failing packument can
	# be the direct dep or its transitive dep.
	assert_output --regexp \
		"(failed to fetch is-odd|failed to resolve dependencies|registry error for is-(odd|number))"
}

@test "AUBE_LOCKFILE=false aube add: installs the dep without writing a lockfile" {
	# Ported from pnpm/test/install/misc.ts:63 ('install --no-lockfile').
	# pnpm's `--no-lockfile` CLI flag has no aube counterpart — aube reads
	# `lockfile` from .npmrc / workspace yaml / env (AUBE_LOCKFILE) only.
	# The behavioral parity is the same: no aube-lock.yaml after the
	# install. is-positive substituted with is-odd. The pnpm-workspace.yaml
	# variant of this test is already covered above (misc.ts:83).
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-no-lockfile-env",
  "version": "0.0.0"
}
JSON

	AUBE_LOCKFILE=false run aube add is-odd
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	assert_file_not_exists aube-lock.yaml
}

@test "aube -r add: fails when no package name is provided in a workspace" {
	# Ported from pnpm/test/install/misc.ts:254 ('pnpm -r add should fail
	# if no package name was provided'). aube reuses the same `aube add`
	# validator under the recursive (-r) entry so the error wording is
	# identical to the single-project case at misc.ts:245 — generic
	# 'no packages specified'.
	mkdir -p project
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project
YAML
	cat >project/package.json <<'JSON'
{
  "name": "project",
  "version": "1.0.0"
}
JSON

	run aube -r add
	assert_failure
	assert_output --partial "no packages specified"
}

@test "AUBE_VIRTUAL_STORE_DIR: relocates the per-project virtual store outside node_modules" {
	# Ported from pnpm/test/install/misc.ts:405 ('using a custom
	# virtual-store-dir location'). pnpm's `--virtual-store-dir=.pnpm`
	# CLI flag has no aube counterpart — virtualStoreDir is npmrc/env-only
	# (sources.cli = [] in settings.toml). The behavioral parity is that
	# the relocated dir houses the dep_path entries and the top-level
	# symlink in node_modules/ still resolves to a real package.
	#
	# rimraf substituted with is-odd. We don't assert on the dep_path
	# subdir naming inside .aube/ — pnpm's test asserts on its specific
	# `<name>@<version>/node_modules/<name>/` shape, and aube's encoded
	# dep_path is BLAKE3-suffixed in the general case (the regression
	# guard is "the relocated dir has content", not the layout details
	# already covered in graph_hash.rs unit tests).
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-virtual-store-relocated",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	AUBE_VIRTUAL_STORE_DIR=.aube run aube install --no-frozen-lockfile
	assert_success
	# The virtual store is now at <project>/.aube/, not the default
	# node_modules/.aube/. Must exist and contain at least one entry.
	assert_dir_exists .aube
	run sh -c 'ls -A .aube | head -1 | grep -q .'
	assert_success
	# Default location must NOT have been written to.
	assert_dir_not_exists node_modules/.aube
	# Top-level entry still resolves to a real package, regardless of
	# where the virtual store lives.
	assert_file_exists node_modules/is-odd/package.json
}

@test "CI=1: install fails when the lockfile drifts from package.json" {
	# Ported from pnpm/test/install/misc.ts:427 ('installing in a CI
	# environment'). rimraf substituted with is-odd (3.0.1 -> 0.1.2 to
	# force a real version drift the lockfile can't satisfy). Covers
	# steps 1-3 of pnpm's test: initial install seeds a lockfile,
	# drifted package.json under CI=1 fails (Frozen mode auto-on), and
	# `--no-frozen-lockfile` bypasses. Pnpm's step 4
	# (`--no-prefer-frozen-lockfile`) has no aube counterpart —
	# `--no-frozen-lockfile` is the canonical bypass.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-ci-frozen",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	# Step 1: seed the lockfile (CI=1 still resolves when no lockfile
	# is present — see install.bats "aube install in CI generates a
	# lockfile when none is present").
	CI=1 run aube install
	assert_success
	assert_file_exists aube-lock.yaml

	# Step 2: drift the manifest. CI=1 makes the default Frozen, so
	# the install must hard-fail rather than silently re-resolve.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-ci-frozen",
  "version": "0.0.0",
  "dependencies": { "is-odd": "0.1.2" }
}
JSON
	CI=1 run aube install
	assert_failure

	# Step 3: explicit --no-frozen-lockfile bypasses the CI default.
	CI=1 run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists node_modules/is-odd/package.json
}

@test "CI=1 + AUBE_PREFER_FROZEN_LOCKFILE=false: env-var override bypasses CI's frozen default" {
	# Ported from pnpm/test/install/misc.ts:457 ('CI mode: frozen-lockfile
	# can be overridden via environment variable'). pnpm uses
	# `pnpm_config_frozen_lockfile=false`; aube has no `frozen-lockfile`
	# setting — the env-equivalent override is `AUBE_PREFER_FROZEN_LOCKFILE=false`,
	# which maps to FrozenMode::No (skip drift checks entirely). Same
	# observable outcome: install succeeds despite drift under CI=1.
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-ci-frozen-env-override",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	CI=1 run aube install
	assert_success

	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-ci-frozen-env-override",
  "version": "0.0.0",
  "dependencies": { "is-odd": "0.1.2" }
}
JSON

	CI=1 AUBE_PREFER_FROZEN_LOCKFILE=false run aube install
	assert_success
	assert_file_exists node_modules/is-odd/package.json
}

@test "engine-strict + workspace: install fails when one project's engines.node mismatches" {
	# Ported from pnpm/test/install/misc.ts:337 ('engine-strict=true:
	# recursive install should fail if the used Node version does not
	# satisfy the Node version specified in engines of any of the
	# workspace projects'). Uses `node-version` override + a high
	# constraint so the test is independent of the host's Node version.
	# Substitutions: is-positive/is-negative -> is-odd/is-even.
	mkdir -p project-1 project-2
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "project-*"
YAML
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-engines-recursive-strict",
  "version": "1.0.0",
  "private": true
}
JSON
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" },
  "engines": { "node": ">=99999" }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON
	cat >.npmrc <<-'RC'
		node-version=18.0.0
		engine-strict=true
	RC

	run aube install --no-frozen-lockfile
	assert_failure
	assert_output --partial "engine-strict"
	assert_output --partial "project-1: wanted node >=99999"
}

@test "engine-strict=false + workspace: install warns but succeeds on workspace project's engines.node mismatch" {
	# Ported from pnpm/test/install/misc.ts:371 ('engine-strict=false:
	# recursive install should not fail if the used Node version does not
	# satisfy the Node version specified in engines of any of the
	# workspace projects'). Same fixture as the strict case above; just
	# drops `engine-strict=true` from .npmrc so the mismatch downgrades to
	# a warning.
	mkdir -p project-1 project-2
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "project-*"
YAML
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-engines-recursive-warn",
  "version": "1.0.0",
  "private": true
}
JSON
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" },
  "engines": { "node": ">=99999" }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON
	echo 'node-version=18.0.0' >.npmrc

	run aube install --no-frozen-lockfile
	assert_success
	assert_output --partial "Unsupported engine"
	assert_output --partial "project-1: wanted node >=99999"
}

@test "engine-strict + workspace: install ignores engines.pnpm" {
	# aube does not honor engines.pnpm: its own version (1.x) lives
	# in a different namespace than pnpm's (10.x+), so honoring
	# `engines.pnpm` against aube's version would emit spurious
	# mismatches on every install of a project that pins a pnpm
	# version. Projects that genuinely want to gate on the running
	# tool can use engines.aube instead. Regression for
	# github.com/jdx/aube/discussions/626.
	#
	# This is a deliberate divergence from pnpm/test/install/misc.ts:303
	# ('recursive install should fail if the used pnpm version does
	# not satisfy the pnpm version specified in engines of any of the
	# workspace projects'). Even under engine-strict, an unsatisfiable
	# engines.pnpm range must not block the install.
	mkdir -p project-1 project-2
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "project-*"
YAML
	cat >package.json <<'JSON'
{
  "name": "pnpm-misc-engines-pnpm-recursive",
  "version": "1.0.0",
  "private": true
}
JSON
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" },
  "engines": { "pnpm": ">=99999" }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON
	echo 'engine-strict=true' >.npmrc

	run aube install --no-frozen-lockfile
	assert_success
	refute_output --partial "wanted pnpm"
}
