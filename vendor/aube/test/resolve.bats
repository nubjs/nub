#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install resolves from scratch when no lockfile" {
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	run aube -v install
	assert_success
	assert_output --partial "No lockfile found"
	assert_output --partial "Resolved"
	assert_output --partial "Wrote aube-lock.yaml"
	assert_file_exists aube-lock.yaml
	assert_dir_exists node_modules
}

@test "resolved packages are requireable" {
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	run aube install
	assert_success
	run node -e "console.log(require('is-odd')(3))"
	assert_success
	assert_output "true"
}

@test "resolver handles transitive deps with multiple versions" {
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1","is-even":"^1.0.0"}}' >package.json
	run aube install
	assert_success
	# Both should work — they use different versions of is-number
	run node -e "console.log(require('is-odd')(3), require('is-even')(4))"
	assert_success
	assert_output "true true"
}

@test "generated lockfile is valid and can be re-used" {
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	# First install: resolve from scratch
	aube install
	assert_file_exists aube-lock.yaml
	# Second install: use existing lockfile (frozen path)
	rm -rf node_modules
	run aube -v install
	assert_success
	# Should NOT say "No lockfile found" — it should use the existing one
	refute_output --partial "No lockfile found"
	assert_output --partial "Lockfile:"
}

@test "generated lockfile contains integrity hashes" {
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	aube install
	run grep "integrity: sha512" aube-lock.yaml
	assert_success
}

@test "aube install --frozen-lockfile fails without lockfile" {
	echo '{"name":"test","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	run aube install --frozen-lockfile
	assert_failure
}
