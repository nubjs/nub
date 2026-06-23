#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_write_unpublishable_pkg() {
	cat >package.json <<-'EOF'
		{
		  "name": "unpublish-smoke",
		  "version": "0.1.0"
		}
	EOF
}

@test "aube unpublish --dry-run uses ./package.json by default" {
	_write_unpublishable_pkg

	run aube unpublish --dry-run --registry=https://r.example.com/
	assert_success
	assert_output --partial "unpublish-smoke@0.1.0"
	assert_output --partial "dry run"
	assert_output --partial "https://r.example.com/"
}

@test "aube unpublish --dry-run with name@version unpublishes a single version" {
	run aube unpublish --dry-run --registry=https://r.example.com/ \
		lodash@4.17.21
	assert_success
	assert_output --partial "lodash@4.17.21"
	assert_output --partial "dry run"
}

@test "aube unpublish bare-name without --force errors" {
	run aube unpublish --dry-run --registry=https://r.example.com/ lodash
	assert_failure
	assert_output --partial "--force"
}

@test "aube unpublish --dry-run --force with bare name reports whole-package intent" {
	run aube unpublish --dry-run --force --registry=https://r.example.com/ lodash
	assert_success
	assert_output --partial "ALL versions"
	assert_output --partial "lodash"
}

@test "aube unpublish --dry-run echoes scoped names in the report" {
	# `--dry-run` only prints the human-readable spec and the registry
	# base URL, not the percent-encoded endpoint path. Encoding is
	# covered by the `encode_package_name` unit tests in commands/mod.rs
	# and exercised live by the `publish` dry-run, which *does* print
	# the URL.
	run aube unpublish --dry-run --registry=https://r.example.com/ \
		'@aube-fixture/demo@1.0.0'
	assert_success
	assert_output --partial "@aube-fixture/demo@1.0.0"
}

@test "aube unpublish errors without an auth token" {
	_write_unpublishable_pkg

	run aube unpublish --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no auth token"
}

@test "aube unpublish errors when ./package.json has no name" {
	cat >package.json <<-'EOF'
		{
		  "version": "0.1.0"
		}
	EOF
	run aube unpublish --dry-run --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no \`name\` field"
}

@test "aube unpublish errors when ./package.json has no version" {
	cat >package.json <<-'EOF'
		{
		  "name": "no-version"
		}
	EOF
	run aube unpublish --dry-run --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no \`version\` field"
}
