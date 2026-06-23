#!/usr/bin/env bats

# `aube doctor` dumps a grouped snapshot of aube's environment and
# project layout, then reports any warnings and errors it detected
# statically. Exits non-zero when the error list is non-empty.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube doctor prints the core sections outside of any project" {
	# Ensure there is no package.json at or above HOME.
	run aube doctor
	assert_success
	assert_output --partial "version:"
	assert_output --partial "dirs:"
	assert_output --partial "registry:"
}

@test "aube doctor surfaces the detected lockfile and package name" {
	cat >package.json <<'JSON'
{
  "name": "doctor-demo",
  "version": "0.1.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	run aube doctor
	assert_success
	assert_output --partial "doctor-demo@0.1.0"
	assert_output --partial "aube-lock.yaml"
	assert_output --partial "No problems found"
}

@test "aube doctor reports broken sibling links as errors and exits 1" {
	cat >package.json <<'JSON'
{
  "name": "doctor-broken",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	rm node_modules/.aube/is-odd@3.0.1/node_modules/is-number
	run aube doctor
	assert_failure
	assert_output --partial "broken dependency link"
	assert_output --partial "aube check"
}

@test "aube doctor --json emits sections, warnings, and errors as JSON" {
	cat >package.json <<'JSON'
{
  "name": "doctor-json",
  "version": "0.0.0"
}
JSON
	run aube doctor --json
	assert_success
	assert_output --partial '"sections"'
	assert_output --partial '"warnings"'
	assert_output --partial '"errors"'
}
