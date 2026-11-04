#!/usr/bin/env bats

# `aube check` walks `node_modules/.aube/` and verifies that every
# installed package can resolve its declared `dependencies` through
# the sibling symlinks Node's module resolver relies on. Exits 1 when
# any link is broken so CI pipelines can gate on it.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube check reports a consistent tree after install" {
	cat >package.json <<'JSON'
{
  "name": "check-consistent",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	run aube check
	assert_success
	assert_output --partial "consistent"
}

@test "aube check exits 1 and names the missing dep when a sibling link is removed" {
	cat >package.json <<'JSON'
{
  "name": "check-broken",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}

JSON
	run aube install
	assert_success

	# is-odd@3 declares a dep on is-number. Remove the sibling symlink
	# from inside the virtual-store cell so the package can no longer
	# resolve it through Node's walk-up.
	rm node_modules/.aube/is-odd@3.0.1/node_modules/is-number
	run aube check
	assert_failure
	assert_output --partial "is-odd@3.0.1"
	assert_output --partial "is-number"
}

@test "aube check reports a dangling importer link when its cell is missing" {
	cat >package.json <<'JSON'
{
  "name": "check-missing-cell",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install
	assert_success

	rm -rf node_modules/.aube/is-odd@3.0.1
	run aube check
	assert_failure
	assert_output --partial "dangling dependency link"
	assert_output --partial "is-odd"
}

@test "aube check from a workspace member scans the shared installation" {
	cat >package.json <<'JSON'
{"name":"check-workspace-root","version":"1.0.0","private":true}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'JSON'
{"name":"check-workspace-a","version":"1.0.0"}
JSON
	cat >packages/b/package.json <<'JSON'
{"name":"check-workspace-b","version":"1.0.0","dependencies":{"is-odd":"3.0.1"}}
JSON
	run aube install
	assert_success

	rm -rf node_modules/.aube/is-odd@3.0.1
	cd packages/a
	run aube check
	assert_failure
	assert_output --partial "packages/b"
	assert_output --partial "dangling dependency link"
	assert_output --partial "is-odd"
}

@test "aube check --json emits a structured report" {
	cat >package.json <<'JSON'
{
  "name": "check-json",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	run aube check --json
	assert_success
	# Sanity-check: the report JSON has the fields we document.
	assert_output --partial '"checked"'
	assert_output --partial '"issues"'
}

@test "aube check is a no-op before the first install" {
	echo '{"name":"x","version":"1.0.0"}' >package.json
	run aube check
	assert_success
	assert_output --partial "checked 0 packages"
}

@test "aube check is a no-op outside any project (no package.json)" {
	# Deliberately no package.json in $TEST_TEMP_DIR.
	run aube check
	assert_success
	assert_output --partial "checked 0 packages"
}
