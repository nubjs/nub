#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube dist-tag --help" {
	run aube dist-tag --help
	assert_success
	assert_output --partial "Manage package distribution tags"
	assert_output --partial "add"
	assert_output --partial "rm"
	assert_output --partial "ls"
}

@test "aube dist-tags is an alias for dist-tag" {
	run aube dist-tags --help
	assert_success
	assert_output --partial "Manage package distribution tags"
}

@test "aube dist-tag add --help shows the version-required hint" {
	run aube dist-tag add --help
	assert_success
	assert_output --partial "name@version"
	assert_output --partial "--otp"
}

@test "aube dist-tag add rejects specs without a version" {
	run aube dist-tag add react next
	assert_failure
	assert_output --partial "expected \`name@version\`"
}

@test "aube dist-tag add rejects empty versions" {
	run aube dist-tag add react@ next
	assert_failure
	assert_output --partial "version is empty"
}

@test "aube dist-tag rm rejects versioned specs" {
	run aube dist-tag rm react@18.0.0 beta
	assert_failure
	assert_output --partial "bare package name"
}

@test "aube dist-tag ls with no arg errors outside a package dir" {
	# common_setup puts us in an isolated tmpdir with no package.json,
	# so reading the project manifest should surface a clean error.
	run aube dist-tag ls
	assert_failure
	assert_output --partial "package.json"
}

@test "aube dist-tag ls with no arg errors when package.json has no name" {
	echo '{}' >package.json
	run aube dist-tag ls
	assert_failure
	assert_output --partial "no \`name\` field"
}

@test "aube dist-tag remove is an alias for rm" {
	run aube dist-tag remove --help
	assert_success
	assert_output --partial "Remove a dist-tag"
	assert_output --partial "--otp"
}
