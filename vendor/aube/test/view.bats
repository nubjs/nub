#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube view --help" {
	run aube view --help
	assert_success
	assert_output --partial "Print package metadata from the registry"
}

@test "aube view <pkg> prints a formatted summary" {
	run aube view is-odd
	assert_success
	# Header line includes name@version, license, deps/versions counts
	assert_output --partial "is-odd@"
	assert_output --partial "deps:"
	assert_output --partial "versions:"
	# Sections present in the fixture packument
	assert_output --partial "dist"
	assert_output --partial "dependencies:"
	assert_output --partial "dist-tags:"
}

@test "aube view <pkg> <field> prints a single field" {
	run aube view is-odd version
	assert_success
	# Fixture packument exposes is-odd@3.0.1 as the latest tag.
	assert_output "3.0.1"
}

@test "aube view <pkg> <nested.field> walks a dotted path" {
	run aube view is-odd dist.tarball
	assert_success
	assert_output --partial "is-odd-"
	assert_output --partial ".tgz"
}

@test "aube view <pkg>@<version> selects a specific version" {
	run aube view is-odd@3.0.1 version
	assert_success
	assert_output "3.0.1"
}

@test "aube view <pkg>@<range> resolves to the highest match" {
	run aube view "is-odd@^3" version
	assert_success
	assert_output "3.0.1"
}

@test "aube view --json dumps the full version metadata" {
	run aube view is-odd --json
	assert_success
	assert_output --partial '"name": "is-odd"'
	assert_output --partial '"version":'
	assert_output --partial '"dist"'
}

@test "aube info is an alias for view" {
	run aube info is-odd version
	assert_success
	assert_output "3.0.1"
}

@test "aube show is an alias for view" {
	run aube show is-odd version
	assert_success
	assert_output "3.0.1"
}

@test "aube view errors cleanly when the package does not exist" {
	run aube view totally-not-a-real-pkg-xyz
	assert_failure
	assert_output --partial "package not found"
}

@test "aube view <scoped-pkg> works" {
	run aube view @sindresorhus/is version
	assert_success
	# Fixture registry has a specific version of @sindresorhus/is; assert
	# it's a non-empty semver-shaped string rather than hard-coding a value
	# that might drift.
	assert_output --regexp '^[0-9]+\.[0-9]+\.[0-9]+'
}
