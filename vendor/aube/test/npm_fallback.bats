#!/usr/bin/env bats
#
# Regression tests for the npm-fallback stub commands (`whoami`, `token`,
# `owner`, `search`, `pkg`, `set-script`, `stage`). pnpm claims these names at the
# CLI surface so they don't fall through to the implicit-script runner;
# aube matches that behavior by bailing with a "not implemented — use
# `npm <cmd>`" error and exiting non-zero. These tests nail down both
# the exit code and the message so a future refactor can't silently
# turn the stubs back into "script not found".

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube whoami prints npm fallback and exits non-zero" {
	run aube whoami
	assert_failure
	assert_output --partial "aube whoami"
	assert_output --partial "is not implemented"
	assert_output --partial "npm whoami"
}

@test "aube whoami swallows extra args instead of clap-erroring first" {
	run aube whoami --json foo bar
	assert_failure
	assert_output --partial "is not implemented"
}

@test "aube token prints npm fallback and exits non-zero" {
	run aube token list
	assert_failure
	assert_output --partial "aube token"
	assert_output --partial "npm token"
}

@test "aube owner prints npm fallback and exits non-zero" {
	run aube owner ls lodash
	assert_failure
	assert_output --partial "aube owner"
	assert_output --partial "npm owner"
}

@test "aube search prints npm fallback and exits non-zero" {
	run aube search react
	assert_failure
	assert_output --partial "aube search"
	assert_output --partial "npm search"
}

@test "aube pkg prints npm fallback and exits non-zero" {
	run aube pkg get name
	assert_failure
	assert_output --partial "aube pkg"
	assert_output --partial "npm pkg"
}

@test "aube set-script prints npm fallback and exits non-zero" {
	run aube set-script build "tsc -p ."
	assert_failure
	assert_output --partial "aube set-script"
	assert_output --partial "npm set-script"
}

@test "aube stage prints npm fallback and exits non-zero" {
	run aube stage list @scope/pkg --json
	assert_failure
	assert_output --partial "aube stage"
	assert_output --partial "is not implemented"
	assert_output --partial "npm stage"
}
