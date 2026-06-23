#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube <script> runs script implicitly" {
	_setup_basic_fixture
	aube install
	run aube hello
	assert_success
	assert_output --partial "hello from aube!"
}

@test "aube <script> with auto-install" {
	_setup_basic_fixture
	# No install first — should auto-install then run
	run aube hello
	assert_success
	assert_output --partial "Auto-installing"
	assert_output --partial "hello from aube!"
}

@test "aube <script> runs node scripts" {
	_setup_basic_fixture
	aube install
	run aube test
	assert_success
	assert_output --partial "is-odd(3): true"
}

@test "aube <unknown> prints help and fails for nonexistent script" {
	_setup_basic_fixture
	aube install
	run aube nonexistent
	assert_failure
	# New behavior: an unknown name (no matching script in package.json
	# and no workspace filter active) prints `aube --help` and bails
	# with "unknown command" instead of the old "script not found".
	# Implicit runs for real scripts still work — covered by the
	# "aube test runs test script" test above.
	assert_output --partial "unknown command: nonexistent"
	assert_output --partial "fast Node.js package manager"
}

@test "bare aube with no args prints help" {
	_setup_basic_fixture
	run aube
	assert_success
	assert_output --partial "Usage: aube"
	assert_not_exists node_modules
}
