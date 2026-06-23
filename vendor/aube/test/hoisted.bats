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
