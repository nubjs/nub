#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install --node-linker=hoisted creates flat node_modules" {
	_setup_basic_fixture
	run aube install --node-linker=hoisted
	assert_success
	assert_dir_exists node_modules
	# Hoisted mode: top-level entries are real directories, not
	# symlinks into .aube/
	run test -d node_modules/is-odd
	assert_success
	run test -L node_modules/is-odd
	assert_failure
	# The package's own package.json is materialized in place.
	assert_file_exists node_modules/is-odd/package.json
}

@test "hoisted mode hoists transitive deps to the top level" {
	_setup_basic_fixture
	run aube install --node-linker=hoisted
	assert_success
	# is-odd@3 → is-number@6; is-even@1 → is-odd@0 → is-number@3.
	# At least one is-number copy lives at the project root.
	assert_dir_exists node_modules/is-number
	assert_file_exists node_modules/is-number/package.json
}

@test "hoisted mode nests conflicting transitive versions" {
	_setup_basic_fixture
	run aube install --node-linker=hoisted
	assert_success
	# is-odd exists both as a direct dep (3.0.1) and as a transitive
	# under is-even (0.1.2). Direct wins the root slot; the
	# conflicting 0.1.2 lives nested under is-even's own node_modules.
	run bash -c "cat node_modules/is-odd/package.json | grep -o '\"version\": *\"3'"
	assert_success
	run test -d node_modules/is-even/node_modules/is-odd
	assert_success
	run bash -c "cat node_modules/is-even/node_modules/is-odd/package.json | grep -o '\"version\": *\"0'"
	assert_success
}

@test "hoisted mode does not create .aube virtual store" {
	_setup_basic_fixture
	run aube install --node-linker=hoisted
	assert_success
	# The isolated virtual store is not written in hoisted mode.
	run test -e node_modules/.aube
	assert_failure
}

@test "hoisted require() resolves through Node's upward walk" {
	_setup_basic_fixture
	run aube install --node-linker=hoisted
	assert_success
	run aube run test
	assert_success
	assert_output --partial "is-odd(3): true"
	assert_output --partial "is-even(4): true"
}

@test "nodeLinker=hoisted in pnpm-workspace.yaml is honored" {
	_setup_basic_fixture
	cat >pnpm-workspace.yaml <<'YAML'
nodeLinker: hoisted
YAML
	run aube install
	assert_success
	run test -d node_modules/is-odd
	assert_success
	run test -L node_modules/is-odd
	assert_failure
}

@test "hoisted workspaces share compatible dependencies at the workspace root" {
	mkdir -p packages/app packages/lib
	cat >package.json <<'JSON'
{"name":"root","private":true}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
nodeLinker: hoisted
YAML
	cat >packages/app/package.json <<'JSON'
{"name":"app","private":true,"dependencies":{"is-number":"7.0.0"}}
JSON
	cat >packages/lib/package.json <<'JSON'
{"name":"lib","private":true,"dependencies":{"is-number":"7.0.0"}}
JSON

	run aube install
	assert_success
	assert_dir_exists node_modules/is-number
	assert_not_exists packages/app/node_modules/is-number
	assert_not_exists packages/lib/node_modules/is-number
	run bash -c '
app=$(cd packages/app && node -p '\''require.resolve("is-number/package.json")'\'')
lib=$(cd packages/lib && node -p '\''require.resolve("is-number/package.json")'\'')
test "$(realpath "$app")" = "$(realpath "$lib")"
'
	assert_success
}

@test "hoisted workspace root placements stay warm before scripts" {
	mkdir -p packages/app
	cat >package.json <<'JSON'
{"name":"root","private":true,"scripts":{"ok":"printf 'ok\\n'"}}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
nodeLinker: hoisted
YAML
	cat >packages/app/package.json <<'JSON'
{"name":"app","private":true,"dependencies":{"is-number":"7.0.0"}}
JSON

	run aube install
	assert_success
	assert_dir_exists node_modules/is-number
	assert_not_exists packages/app/node_modules/is-number

	# `assert_line` rather than `assert_output`: `aube run` also echoes the
	# `$ <cmd>` line. Using `-s` to get an exact match would mute the
	# "Auto-installing" notice this test is here to refute.
	run aube run ok
	assert_success
	assert_line "ok"
	refute_output --partial "Auto-installing"
	run aube run ok
	assert_success
	assert_line "ok"
	refute_output --partial "Auto-installing"
}

@test "hoistingLimits=workspaces keeps dependencies under each workspace" {
	mkdir -p packages/app packages/lib
	cat >package.json <<'JSON'
{"name":"root","private":true}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
nodeLinker: hoisted
hoistingLimits: workspaces
YAML
	cat >packages/app/package.json <<'JSON'
{"name":"app","private":true,"dependencies":{"is-number":"7.0.0"}}
JSON
	cat >packages/lib/package.json <<'JSON'
{"name":"lib","private":true,"dependencies":{"is-number":"7.0.0"}}
JSON

	run aube install
	assert_success
	assert_not_exists node_modules/is-number
	assert_dir_exists packages/app/node_modules/is-number
	assert_dir_exists packages/lib/node_modules/is-number
}

@test "--node-linker=pnp is rejected" {
	_setup_basic_fixture
	run aube install --node-linker=pnp
	assert_failure
	assert_output --partial "node-linker=pnp is not supported"
}

@test "nodeLinker: pnp in pnpm-workspace.yaml is rejected" {
	_setup_basic_fixture
	cat >pnpm-workspace.yaml <<'YAML'
nodeLinker: pnp
YAML
	run aube install
	assert_failure
	assert_output --partial "node-linker=pnp is not supported"
}

@test "--node-linker=garbage errors with a clear message" {
	_setup_basic_fixture
	run aube install --node-linker=garbage
	assert_failure
	assert_output --partial "unknown --node-linker value"
}

# Regression: hoisted layout did not link the bins of hoisted transitive
# deps. `link_bins` walked only the root importer's direct deps, and the
# per-dep pass short-circuited under hoisted, so a transitive hoisted into
# the shared `node_modules/` had its bins linked nowhere — every lifecycle
# script invoking one exited 127. pnpm's hoisted layout links the bins of
# every package sitting in a `node_modules/` into that directory's `.bin`.
#
# `aube-test-transitive-consumer` postinstalls `aube-transitive-bin-probe`,
# a bin owned by its transitive `aube-test-transitive-bin`. The probe writes
# a marker into `$INIT_CWD`, so the marker proves the bin resolved on PATH
# during the script; the `.bin` assertion pins the layout independently of
# whether any script ran.
@test "hoisted mode links bins of hoisted transitive deps" {
	cat >package.json <<'JSON'
{
  "name": "hoisted-transitive-bin-test",
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
	run aube install --node-linker=hoisted
	assert_success
	assert_file_exists aube-transitive-bin-probe.txt
	assert_file_exists node_modules/.bin/aube-transitive-bin-probe
}
