#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# -- Root lifecycle hooks run during `aube install` ---------------------------

@test "aube install runs root preinstall hook" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'require(\"fs\").writeFileSync(\"preinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube -v install
	assert_success
	assert_file_exists preinstall.marker
}

@test "aube install runs root postinstall hook after deps are linked" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"is-odd\"); require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube -v install
	assert_success
	assert_file_exists postinstall.marker
}

@test "aube install runs prepare hook last" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'require(\"fs\").writeFileSync(\"order.log\", \"pre\\n\", {flag: \"a\"})'",
    "postinstall": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"post\\n\")'",
    "prepare": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"prepare\\n\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	# Exact order: pre → post → prepare
	run cat order.log
	assert_output "pre
post
prepare"
}

@test "workspace install runs member postinstall hooks after member deps are linked" {
	cat >package.json <<'JSON'
{
  "name": "workspace-lifecycle-root",
  "version": "1.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/app
	cat >packages/app/package.json <<'JSON'
{
  "name": "workspace-lifecycle-app",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"is-odd\"); require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists packages/app/postinstall.marker
}

@test "workspace member onlyBuiltDependencies bare name skips source dep postinstall" {
	cat >package.json <<'JSON'
{
  "name": "workspace-build-policy-root",
  "version": "1.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/app/dep-with-build
	cat >packages/app/dep-with-build/package.json <<'JSON'
{
  "name": "member-dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >packages/app/package.json <<'JSON'
{
  "name": "workspace-build-policy-app",
  "version": "1.0.0",
  "dependencies": {
    "member-dep-with-build": "file:./dep-with-build"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["member-dep-with-build"]
  }
}
JSON
	run aube install
	assert_success
	assert_file_not_exists packages/app/node_modules/member-dep-with-build/built.marker
	assert_output --partial "member-dep-with-build@file:packages/app/dep-with-build"
}

@test "requiredScripts enforces root package scripts" {
	cat >.npmrc <<'EOF'
requiredScripts=build,test
EOF
	cat >package.json <<'JSON'
{
  "name": "required-scripts-test",
  "version": "1.0.0",
  "scripts": {
    "build": "echo build"
  }
}
JSON
	run aube install
	assert_failure
	assert_output --partial "requiredScripts check failed"
	assert_output --partial ". is missing \`test\`"
}

@test "strictDepBuilds fails for unreviewed dependency build scripts" {
	cat >.npmrc <<'EOF'
strictDepBuilds=true
EOF
	mkdir -p dep-with-build
	cat >dep-with-build/package.json <<'JSON'
{
  "name": "dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "install": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >package.json <<'JSON'
{
  "name": "strict-dep-builds-test",
  "version": "1.0.0",
  "dependencies": {
    "dep-with-build": "file:./dep-with-build"
  }
}
JSON
	cp package.json package.json.before
	run aube install
	assert_failure
	assert_output --partial "dependencies with build scripts must be reviewed"
	assert_output --partial "dep-with-build@file:./dep-with-build"
	# Diverges from pnpm: aube does not auto-seed an `allowBuilds`
	# placeholder. The manifest is left exactly as the user wrote it.
	assert_file_not_exists aube-workspace.yaml
	run diff -u package.json.before package.json
	assert_success
}

@test "--config.strict-dep-builds=true forces strictDepBuilds for one invocation" {
	# pnpm-style generic `--config.<key>=<value>` flag should set
	# `strictDepBuilds` even though the setting declares no
	# command-specific CLI alias.
	mkdir -p dep-with-build
	cat >dep-with-build/package.json <<'JSON'
{
  "name": "dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "install": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >package.json <<'JSON'
{
  "name": "config-flag-test",
  "version": "1.0.0",
  "dependencies": {
    "dep-with-build": "file:./dep-with-build"
  }
}
JSON
	run aube install --config.strict-dep-builds=true
	assert_failure
	assert_output --partial "dependencies with build scripts must be reviewed"
	assert_output --partial "dep-with-build@file:./dep-with-build"
}

@test "strictDepBuilds=false keeps unreviewed dependency build scripts skipped" {
	cat >.npmrc <<'EOF'
strictDepBuilds=false
EOF
	mkdir -p dep-with-build
	cat >dep-with-build/package.json <<'JSON'
{
  "name": "dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "install": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >package.json <<'JSON'
{
  "name": "strict-dep-builds-off-test",
  "version": "1.0.0",
  "dependencies": {
    "dep-with-build": "file:./dep-with-build"
  }
}
JSON
	run aube install
	assert_success
	[ ! -e node_modules/dep-with-build/built.marker ]
}

@test "sideEffectsCacheReadonly restores but does not write dependency build cache" {
	cat >.npmrc <<'EOF'
sideEffectsCacheReadonly=true
EOF
	mkdir -p dep-with-build
	cat >dep-with-build/package.json <<'JSON'
{
  "name": "dep-with-build",
  "version": "1.0.0",
  "scripts": {
    "install": "node -e 'require(\"fs\").writeFileSync(\"built.marker\", \"ran\")'"
  }
}
JSON
	cat >package.json <<'JSON'
{
  "name": "side-effects-readonly-test",
  "version": "1.0.0",
  "dependencies": {
    "dep-with-build": "file:./dep-with-build"
  },
  "pnpm": {
    "onlyBuiltDependencies": ["dep-with-build"]
  }
}
JSON
	run aube install --dangerously-allow-all-builds
	assert_success
	assert_file_exists node_modules/dep-with-build/built.marker
	[ ! -e node_modules/side-effects-v1 ]
}

@test "aube install: failed dep build retries on next install (rollback contract)" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:108
	# ('dependency should not be added to package.json and lockfile if
	# it was not built successfully'). Reframed per the triage doc:
	# aube's CAS architecture means the literal pnpm assertion (dep dir
	# is removed after failure) doesn't translate, but the underlying
	# contract — "after a failed install, the next `aube install`
	# retries and still fails until the user fixes the manifest" — is
	# enforced by the state-not-written-on-failure model at
	# install/mod.rs:1342-1364. The fixture
	# `@pnpm.e2e/aube-test-failing-install` is a minimal package whose
	# `install` script is `exit 1` — guaranteed to fail every time.
	# `--dangerously-allow-all-builds` is the CLI form (no .npmrc setting
	# exists for this); precedent: test/allow_builds.bats:87.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-rollback",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/aube-test-failing-install": "1.0.0"
  }
}
JSON
	# First install: build script exits 1, install fails.
	run aube install --dangerously-allow-all-builds
	assert_failure
	# State must not have been written — that's what makes the next
	# install try again instead of treating the prior failure as success.
	assert [ ! -e node_modules/.aube-state ]

	# Second install: regression guard. Without the
	# state-not-written-on-failure invariant, aube would silently mark
	# the prior failed run as fresh and skip the build script, making
	# the broken dep look installed.
	run aube install --dangerously-allow-all-builds
	assert_failure

	# Drop the broken dep — aube install must succeed cleanly. Confirms
	# the failure didn't leave the project in an unrecoverable state.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-rollback",
  "version": "1.0.0"
}
JSON
	run aube install
	assert_success
}

@test "aube install fails fast if a root lifecycle script exits non-zero" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "exit 17"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_failure
	assert_output --partial "preinstall"
	# node_modules should NOT have been populated — preinstall runs before link
	assert [ ! -e node_modules/is-odd ]
}

@test "aube install --ignore-scripts skips root lifecycle hooks" {
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'require(\"fs\").writeFileSync(\"should-not-exist\", \"x\")'",
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"should-not-exist\", \"x\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install --ignore-scripts
	assert_success
	assert [ ! -e should-not-exist ]
	# Deps should still be installed though
	assert_file_exists node_modules/is-odd/package.json
}

@test "root hooks can use binaries from node_modules/.bin via PATH" {
	# Classic pnpm workflow: postinstall invokes a tool installed as a dep.
	# Use is-odd's CLI? — it doesn't have one. Instead use `which` on a
	# known binary we install. Easier: touch a marker from a script and
	# verify PATH contains node_modules/.bin.
	cat >package.json <<'JSON'
{
  "name": "lifecycle-test",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "echo \"$PATH\" > path.log"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	run cat path.log
	assert_output --partial "node_modules/.bin"
}

@test "root hooks receive npm_package_* env vars" {
	cat >package.json <<'JSON'
{
  "name": "env-test-pkg",
  "version": "1.2.3",
  "scripts": {
    "postinstall": "node -e 'console.log(process.env.npm_package_name + \"@\" + process.env.npm_package_version)'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "env-test-pkg@1.2.3"
}

@test "install hooks are a no-op when script field is undefined" {
	# Just asserting that nothing weird happens when there's nothing to run.
	_setup_basic_fixture
	run aube install
	assert_success
	# No mention of "Running" anything since basic fixture has no lifecycle scripts
	refute_output --partial "Running preinstall"
	refute_output --partial "Running postinstall"
}

# -- Dep lifecycle scripts can invoke transitive bins -------------------------

# Regression test for the bug where a dep's postinstall couldn't spawn
# a bin declared in the dep's own `dependencies` (e.g.
# `unrs-resolver`'s postinstall calling `prebuild-install`). The fix
# writes a per-dep `.bin/` at `.aube/<subdir>/node_modules/.bin/` and
# prepends it to PATH when the dep's lifecycle scripts run.
#
# Fixtures: `aube-test-transitive-consumer` depends on
# `aube-test-transitive-bin` (which ships a bin named
# `aube-transitive-bin-probe`) and has `postinstall:
# "aube-transitive-bin-probe"`. The probe writes
# `aube-transitive-bin-probe.txt` into `$INIT_CWD` when it runs, so
# the marker's presence proves the transitive bin was reachable on
# PATH during the dep's lifecycle script.
@test "dep postinstall can invoke a transitive-dep bin by bare name" {
	cat >package.json <<'JSON'
{
  "name": "transitive-bin-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-transitive-consumer": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-transitive-consumer": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-transitive-bin-probe.txt
}

# -- Ported from pnpm/test/install/lifecycleScripts.ts ------------------------
#
# Existing aube tests above cover most of pnpm's filesystem-marker assertions
# (preinstall ran / postinstall ran / prepare ran / exit-non-zero fails install
# / --ignore-scripts skips hooks / npm_package_* env vars). The block below
# adds the orthogonal stdout-visibility assertions from pnpm's suite (the
# script's echo reaches the user), plus three parity tests that previously
# documented divergences and now ride the corresponding fixes:
# `npm_config_user_agent` is exported, and root postinstall/prepare no longer
# fire on `aube add <pkg>`.

@test "aube install: preinstall script stdout reaches the user" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:43
	# ('preinstall is executed before general installation').
	# Complements the existing filesystem-marker test by also asserting
	# that the script's echoed output makes it through aube's progress UI.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-preinstall-stdout",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "echo HELLO_FROM_PREINSTALL"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "HELLO_FROM_PREINSTALL"
}

@test "aube install: postinstall script stdout reaches the user" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:56
	# ('postinstall is executed after general installation').
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-postinstall-stdout",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "echo HELLO_FROM_POSTINSTALL"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "HELLO_FROM_POSTINSTALL"
}

@test "aube install: prepare script stdout reaches the user (argumentless install)" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:95
	# ('prepare is executed after argumentless installation').
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-prepare-stdout",
  "version": "1.0.0",
  "scripts": {
    "prepare": "echo HELLO_FROM_PREPARE"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "HELLO_FROM_PREPARE"
}

@test "aube: lifecycle scripts receive npm_config_user_agent" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:29
	# ('lifecycle script runs with the correct user agent').
	# aube exports the same env var so dep build scripts (husky,
	# unrs-resolver, node-pre-gyp, etc.) can detect the running PM.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-user-agent",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "node -e 'console.log(\"UA=\" + (process.env.npm_config_user_agent || \"\"))'"
  }
}
JSON
	run aube install
	assert_success
	# pnpm asserts the user agent starts with `${pkgName}/${pkgVersion}`.
	assert_output --regexp "UA=aube/[0-9]+\.[0-9]+\.[0-9]+"
}

@test "aube install: lifecycle hooks export the full npm_* env set (pnpm parity)" {
	# Companion to the run-path test in run.bats: install lifecycle hooks
	# must see the same npm_* surface pnpm provides — npm_execpath,
	# npm_node_execpath, npm_package_json, npm_command (=install here),
	# npm_config_node_gyp, the deep-flattened engines/config/bin, and the
	# raw npm_lifecycle_script. Tools like node-pre-gyp / husky branch on
	# these during a dependency build.
	cat >package.json <<'JSON'
{
  "name": "@scope/install-env-probe",
  "version": "4.5.6",
  "engines": { "node": ">=18.0.0" },
  "config": { "port": "8080" },
  "bin": { "probe-cli": "./cli.js" },
  "scripts": {
    "postinstall": "node -e 'for (const k of [\"npm_execpath\",\"npm_node_execpath\",\"npm_package_json\",\"npm_command\",\"npm_config_node_gyp\",\"npm_package_engines_node\",\"npm_package_config_port\",\"npm_package_bin_probe_cli\",\"npm_lifecycle_script\"]) console.log(k + \"=\" + (process.env[k] || \"\"))'"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "npm_command=install"
	assert_output --regexp "npm_execpath=[^[:space:]]*aube"
	assert_output --regexp "npm_node_execpath=[^[:space:]]+"
	assert_output --regexp "npm_package_json=[^[:space:]]*package\.json"
	assert_output --regexp "npm_config_node_gyp=[^[:space:]]*node-gyp\.js"
	assert_output --partial "npm_package_engines_node=>=18.0.0"
	assert_output --partial "npm_package_config_port=8080"
	assert_output --partial "npm_package_bin_probe_cli=./cli.js"
	assert_output --regexp "npm_lifecycle_script=.*node -e"
}

@test "aube add: root postinstall is NOT triggered when adding a named dep" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:69
	# ('postinstall is not executed after named installation').
	# pnpm's contract: lifecycle hooks only run during an argumentless
	# `install` — `pnpm install <pkg>` (i.e. `aube add <pkg>`) skips
	# them so adding a single dep doesn't re-run codegen / build steps.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-named-postinstall",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  }
}
JSON
	run aube add is-odd@3.0.1
	assert_success
	assert [ ! -e postinstall.marker ]
}

@test "aube add: root prepare is NOT triggered when adding a named dep" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:82
	# ('prepare is not executed after installation with arguments').
	# Same contract as the postinstall case above.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-named-prepare",
  "version": "1.0.0",
  "scripts": {
    "prepare": "node -e 'require(\"fs\").writeFileSync(\"prepare.marker\", \"ran\")'"
  }
}
JSON
	run aube add is-odd@3.0.1
	assert_success
	assert [ ! -e prepare.marker ]
}

@test "aube remove: root postinstall is NOT triggered" {
	# Same pnpm contract as the `aube add` cases — root hooks fire only
	# on argumentless `aube install`. `pnpm remove <pkg>` is a chained
	# operation that must not re-run them.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-remove",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	# Seed node_modules with --ignore-scripts so the marker isn't written
	# during setup, then exercise `aube remove` under regular settings.
	run aube install --ignore-scripts
	assert_success
	rm -f postinstall.marker

	run aube remove is-odd
	assert_success
	assert [ ! -e postinstall.marker ]
}

@test "aube update: root postinstall is NOT triggered" {
	# Same pnpm contract — `aube update` is a chained operation.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-update",
  "version": "1.0.0",
  "scripts": {
    "postinstall": "node -e 'require(\"fs\").writeFileSync(\"postinstall.marker\", \"ran\")'"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install --ignore-scripts
	assert_success
	rm -f postinstall.marker

	run aube update
	assert_success
	assert [ ! -e postinstall.marker ]
}

# -- Dep build-policy ports from pnpm/test/install/lifecycleScripts.ts --------
#
# Cover aube's `allowBuilds` review machinery and `--allow-build` CLI
# flag. Diverges from pnpm: aube never writes a "set this to true or
# false" placeholder into the user's manifest. Aube's strict-dep-builds
# error message also differs from pnpm's ("dependencies with build
# scripts must be reviewed" vs "Ignored build scripts:").

@test "aube add leaves package.json untouched for unreviewed dep build scripts" {
	# Diverges from pnpm/test/install/lifecycleScripts.ts:260
	# ('ignored builds are auto-populated as placeholders in allowBuilds').
	# Aube does not auto-seed — the manifest is left alone except for
	# the new dep entry under `dependencies`.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allowbuilds-seed",
  "version": "1.0.0"
}
JSON
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_success
	assert_file_not_exists pnpm-workspace.yaml
	assert_file_not_exists aube-workspace.yaml
	# `dependencies` entry exists, but no `allowBuilds` map is written.
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example"' package.json
	assert_success
	run grep -F 'allowBuilds' package.json
	assert_failure
}

@test "aube add does not seed allowBuilds even when a workspace yaml already exists" {
	# Diverges from pnpm/test/install/lifecycleScripts.ts:268
	# ('auto-populated placeholders are merged with existing allowBuilds').
	# Pre-existing approval is preserved verbatim; aube does not append
	# the new build-script dep to the map.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allowbuilds-merge",
  "version": "1.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
allowBuilds:
  "@pnpm.e2e/install-script-example": true
YAML
	cp pnpm-workspace.yaml pnpm-workspace.yaml.before
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_success
	# Existing yaml is unchanged.
	run diff -u pnpm-workspace.yaml.before pnpm-workspace.yaml
	assert_success
}

@test "aube add fails with strictDepBuilds=true when a dep has unreviewed build scripts" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:226
	# ('throw an error when strict-dep-builds is true and there are
	# ignored scripts'). pnpm's error reads "Ignored build scripts:" and
	# uses `--config.strict-dep-builds=true`; aube has no CLI surface
	# for the setting (reads it from .npmrc / pnpm-workspace.yaml / env)
	# and surfaces a different error string. Common contract: install
	# fails, but the dep + lockfile are still written so the user can
	# add the package to `allowBuilds` and re-run.
	# Append (don't overwrite) so the registry= line _common_setup wrote
	# survives when AUBE_TEST_REGISTRY is set.
	echo "strictDepBuilds=true" >>.npmrc
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-strict-dep-builds-registry",
  "version": "1.0.0"
}
JSON
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_failure
	assert_output --partial "dependencies with build scripts must be reviewed"
	assert_output --partial "@pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0"
	# Dep is still written to package.json + lockfile (matches pnpm).
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0"' package.json
	assert_success
	assert_file_exists aube-lock.yaml
	# Aube does NOT auto-seed an allowBuilds placeholder (diverges from pnpm).
	run grep -F 'allowBuilds' package.json
	assert_failure
}

@test "strictDepBuilds fails even when side-effects are already cached" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:303
	# ('strictDepBuilds fails for packages with cached side-effects (#11035)').
	# Regression: a previously-approved build populates the side-effects
	# cache. After removing the approval, the second install must still
	# fail under strictDepBuilds=true rather than silently restoring the
	# cached output.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-strict-cached",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "@pnpm.e2e/pre-and-postinstall-scripts-example": true
    }
  }
}
JSON
	# First install: build runs, side-effects cache populated.
	run aube install
	assert_success
	assert_file_exists node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-postinstall.js

	# Drop the approval and turn on strictDepBuilds. The cached output
	# is in the store, but aube must still fail rather than silently
	# restore it. Append so the registry= line survives.
	echo "strictDepBuilds=true" >>.npmrc
	echo "optimisticRepeatInstall=false" >>.npmrc
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-strict-cached",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0"
  }
}
JSON
	run aube install
	assert_failure
	assert_output --partial "dependencies with build scripts must be reviewed"
	assert_output --partial "@pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0"
}

@test "aube add --allow-build=<pkg> selectively pre-approves a dep's build scripts" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:149
	# ('selectively allow scripts in some dependencies by --allow-build flag').
	# Adds two build-script packages and pre-approves one via the flag —
	# only the named one runs its build, the other is left alone (aube
	# does not auto-seed a placeholder for it).
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-selective",
  "version": "1.0.0"
}
JSON
	run aube add \
		--allow-build=@pnpm.e2e/install-script-example \
		@pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0 \
		@pnpm.e2e/install-script-example
	assert_success
	# Approved dep ran its install script.
	assert_file_exists node_modules/@pnpm.e2e/install-script-example/generated-by-install.js
	# Unapproved dep did NOT run pre/post-install scripts.
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-preinstall.js ]
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-postinstall.js ]
	# Manifest state: approved entry is `true`; unapproved dep is NOT
	# added to `allowBuilds` (diverges from pnpm — aube leaves the
	# manifest alone).
	run grep -F '"@pnpm.e2e/install-script-example": true' package.json
	assert_success
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example"' package.json
	assert_success
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example": "set this to true or false"' package.json
	assert_failure
}

@test "aube add --deny-build=<pkg> reviews and skips a dep's build scripts" {
	cat >package.json <<'JSON'
{
  "name": "aube-lifecycle-deny-build-selective",
  "version": "1.0.0"
}
JSON
	AUBE_STRICT_DEP_BUILDS=true run aube add \
		--deny-build=@pnpm.e2e/install-script-example \
		@pnpm.e2e/install-script-example
	assert_success
	refute_output --partial "must be reviewed before install"
	refute_output --partial "ignored build scripts"
	assert [ ! -e node_modules/@pnpm.e2e/install-script-example/generated-by-install.js ]
	run grep -F '"@pnpm.e2e/install-script-example": false' package.json
	assert_success
}

@test "aube add rejects packages listed in both allow-build and deny-build" {
	run aube add \
		--allow-build=@pnpm.e2e/install-script-example \
		--deny-build=@pnpm.e2e/install-script-example \
		@pnpm.e2e/install-script-example
	assert_failure
	assert_output --partial "ERR_AUBE_CONFLICTING_BUILD_FLAGS"
	assert_output --partial "--allow-build and --deny-build both name the same package(s)"
	assert_output --partial "Each package may only appear in one flag"
}

@test "aube add --allow-build with no value errors and points at the = syntax" {
	# Bare `--allow-build` is rejected by clap before it reaches our
	# validator, because the arg has `require_equals = true` and no
	# `default_missing_value`. Clap's diagnostic — "equal sign is
	# needed when assigning values to '--allow-build=<PKG>'" — points
	# the user straight at the correct syntax, where the prior
	# pnpm-verbatim "missing a package name" wording was ambiguous
	# (Discussion #655). The explicit empty form `--allow-build=`
	# still routes through `parse_allow_build_value` and keeps the
	# pnpm-verbatim wording; see the companion test below.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-bare",
  "version": "1.0.0"
}
JSON
	run aube add @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0 --allow-build
	assert_failure
	assert_output --partial "equal sign is needed when assigning values to '--allow-build=<PKG>'"
	# Build did not run.
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-preinstall.js ]
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-postinstall.js ]
}

@test "aube add --allow-build= (explicit empty equals) errors with pnpm's verbatim wording" {
	# Companion to the bare-flag test above. `--allow-build=` parses to
	# the empty string, which `parse_allow_build_value` rejects with
	# the same pnpm wording — covers the form a user might type when
	# pasting from a shell variable that came back empty.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-empty-equals",
  "version": "1.0.0"
}
JSON
	run aube add --allow-build= @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_failure
	assert_output --partial "The --allow-build flag is missing a package name."
	assert_output --partial "Please specify the package name(s) that are allowed to run installation scripts."
	# Build did not run, manifest untouched.
	assert [ ! -e node_modules/@pnpm.e2e/pre-and-postinstall-scripts-example/generated-by-preinstall.js ]
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example"' package.json
	assert_failure
}

@test "aube add --allow-build (space form) does not silently swallow the next positional" {
	# Regression: without `require_equals = true`, clap would greedily
	# consume the next non-flag token as the allow-build value —
	# `aube add --allow-build esbuild some-pkg` would silently parse
	# `esbuild` as the value and leave the positional packages list
	# short. `require_equals = true` forces the `=` syntax, so the
	# diagnostic is clap's "equal sign is needed" error instead of a
	# silent no-op.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-no-swallow",
  "version": "1.0.0"
}
JSON
	run aube add --allow-build @pnpm.e2e/install-script-example @pnpm.e2e/pre-and-postinstall-scripts-example@1.0.0
	assert_failure
	assert_output --partial "equal sign is needed when assigning values to '--allow-build=<PKG>'"
	# Neither package was installed.
	run grep -F '"@pnpm.e2e/install-script-example"' package.json
	assert_failure
	run grep -F '"@pnpm.e2e/pre-and-postinstall-scripts-example"' package.json
	assert_failure
}

@test "aube add --allow-build=<pkg> writes to workspace root under --filter" {
	# Regression: in the workspace-filter path (`aube add --filter=<sel>
	# <pkg> --allow-build=<pkg>`), the `--allow-build` flag was silently
	# dropped — no approval was written. Pin that the filtered path
	# writes the approval to the workspace root yaml (not the child).
	mkdir -p packages/app
	cat >package.json <<'JSON'
{
  "name": "root",
  "version": "1.0.0",
  "private": true
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "packages/*"
YAML
	cat >packages/app/package.json <<'JSON'
{
  "name": "@scope/app",
  "version": "1.0.0"
}
JSON

	run aube --filter '@scope/app' add \
		--allow-build=@pnpm.e2e/install-script-example \
		@pnpm.e2e/install-script-example
	assert_success
	# Workspace yaml has the approval; child manifest has the dep.
	run grep -E "@pnpm\.e2e/install-script-example['\"]?: true" pnpm-workspace.yaml
	assert_success
	run grep -F '"@pnpm.e2e/install-script-example"' packages/app/package.json
	assert_success
	# Build actually ran — `generated-by-install.js` only exists when
	# the dep's lifecycle scripts were allowed.
	assert_file_exists node_modules/.aube/@pnpm.e2e+install-script-example@1.0.0/node_modules/@pnpm.e2e/install-script-example/generated-by-install.js
}

@test "aube add --allow-build is rejected when combined with --no-save" {
	# Same conflict pnpm enforces (and that --save-catalog already
	# enforces in aube): --no-save's restore path snapshots only
	# package.json + the lockfile, but --allow-build can land in the
	# workspace yaml — combining them would leak an orphaned approval.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-allow-build-no-save",
  "version": "1.0.0"
}
JSON
	run aube add --no-save --allow-build=@pnpm.e2e/install-script-example @pnpm.e2e/install-script-example
	assert_failure
	assert_output --partial "--allow-build"
	assert_output --partial "--no-save"
	# Manifest untouched — clap rejected the combo before any write.
	run grep -F '"@pnpm.e2e/install-script-example"' package.json
	assert_failure
}

@test "aube install re-emits ignored-build-scripts warning on repeat install" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:245
	# ('warning is shown when an install with --no-frozen-lockfile reuses
	# an existing node_modules with ignored build scripts').
	# pnpm's assertion text reads "Ignored build scripts:"; aube's reads
	# "ignored build scripts for N package(s):" — the contract being
	# tested is identical (the warning still fires on a repeat install
	# that hits the warm-path short-circuit), wording substituted to
	# match aube's canonical phrasing per CLAUDE.md.
	cat >package.json <<'JSON'
{
  "name": "pnpm-lifecycle-repeat-install-warn",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0"
  }
}
JSON
	# First install: warning fires from the full pipeline.
	run aube install
	assert_success
	assert_output --partial "ignored build scripts"
	assert_output --partial "@pnpm.e2e/pre-and-postinstall-scripts-example"
	# Second install: state matches, warm-path short-circuit fires. The
	# unreviewed-builds set persisted in `.aube-state` lets the warning
	# re-emit so the user keeps seeing the nudge until they review.
	run aube install
	assert_success
	assert_output --partial "Already up to date"
	assert_output --partial "ignored build scripts"
	assert_output --partial "@pnpm.e2e/pre-and-postinstall-scripts-example"
}

@test "aube add --allow-build=<pkg> flips an existing allowBuilds: <pkg>: false to true" {
	# pnpm errors when `--allow-build=<pkg>` collides with an existing
	# `allowBuilds: <pkg>: false`. aube flips the value instead
	# (Discussion #655): the user passed the flag deliberately, so
	# treating it as a conflict just forces them to hand-edit the yaml
	# to get the same effect.
	cat >package.json <<'JSON'
{
  "name": "aube-lifecycle-allow-build-flip",
  "version": "1.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
allowBuilds:
  "@pnpm.e2e/install-script-example": false
YAML
	run aube add \
		--allow-build=@pnpm.e2e/install-script-example \
		@pnpm.e2e/install-script-example@1.0.0
	assert_success
	# The yaml entry was flipped to `true`.
	run grep -E "@pnpm\.e2e/install-script-example['\"]?:\s*true" pnpm-workspace.yaml
	assert_success
	# And the dep is in the manifest.
	run grep -F '"@pnpm.e2e/install-script-example"' package.json
	assert_success
}

# Ported from pnpm/test/install/lifecycleScripts.ts:179
# (preinstall script does not trigger verify-deps-before-run, pnpm/pnpm#8954).
# Substitution: cowsay@1.5.0 → is-odd (in-tree fixture), and pnpm's
# `--config.verify-deps-before-run=error` flag is preserved verbatim via
# aube's pnpm-compat `--config.<key>=<value>` parser.
@test "preinstall script does not trigger verify-deps-before-run" {
	cat >package.json <<'JSON'
{
  "name": "preinstall-script-does-not-trigger-verify-deps-before-run",
  "version": "1.0.0",
  "private": true,
  "scripts": {
    "sayHello": "echo hello world",
    "preinstall": "aube run sayHello"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube --config.verify-deps-before-run=error install
	assert_success
	assert_output --partial "hello world"
}

# Ported from pnpm/test/install/lifecycleScripts.ts:200
# (preinstall and postinstall scripts do not trigger verify-deps-before-run
# when using settings from a config file, pnpm/pnpm#10060).
# Without the lifecycle-context guard, the inner `aube run` deadlocks on the
# project lock the outer install holds. The test fails fast (60s ceiling) if
# the deadlock returns.
@test "preinstall + postinstall scripts do not trigger verify-deps-before-run via workspace yaml" {
	cat >package.json <<'JSON'
{
  "name": "preinstall-script-does-not-trigger-verify-deps-before-run-config-file",
  "version": "1.0.0",
  "private": true,
  "scripts": {
    "sayHello": "echo hello world",
    "preinstall": "aube run sayHello",
    "postinstall": "aube run sayHello"
  },
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
verifyDepsBeforeRun: install
YAML
	# `timeout(1)` is GNU coreutils — Linux ships it as `timeout`,
	# macOS only ships it as `gtimeout` (after `brew install
	# coreutils`) and not at all on a stock install. Pick whichever
	# the host has; if neither is present (macOS without coreutils),
	# fall back to `aube install` direct. The bats wall-clock cap
	# (set in CI) catches the deadlock-regression case the timeout
	# is meant to guard against — a stock-macOS dev who hits a
	# regression locally will need to ctrl-c, which matches the
	# existing pre-fix behavior anyway.
	if command -v timeout >/dev/null 2>&1; then
		run timeout 60 aube install
	elif command -v gtimeout >/dev/null 2>&1; then
		run gtimeout 60 aube install
	else
		run aube install
	fi
	assert_success
	assert_output --partial "hello world"
}
