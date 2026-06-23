#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
	# macOS symlinks /var -> /private/var, so $PWD and the kernel-reported
	# cwd disagree. aube prints the kernel cwd, so resolve $PWD to its
	# physical path before comparing.
	PHYS_PWD="$(pwd -P)"
}

teardown() {
	_common_teardown
}

@test "aube root prints <cwd>/node_modules" {
	run aube root
	assert_success
	assert_output "$PHYS_PWD/node_modules"
}

@test "aube bin prints <cwd>/node_modules/.bin" {
	run aube bin
	assert_success
	assert_output "$PHYS_PWD/node_modules/.bin"
}

@test "aube root works without a package.json" {
	run aube root
	assert_success
	assert_output "$PHYS_PWD/node_modules"
}

@test "aube bin works without a package.json" {
	run aube bin
	assert_success
	assert_output "$PHYS_PWD/node_modules/.bin"
}

@test "aube root honors global -C flag" {
	mkdir sub
	run aube -C sub root
	assert_success
	assert_output "$PHYS_PWD/sub/node_modules"
}

@test "aube bin honors global -C flag" {
	mkdir sub
	run aube -C sub bin
	assert_success
	assert_output "$PHYS_PWD/sub/node_modules/.bin"
}
