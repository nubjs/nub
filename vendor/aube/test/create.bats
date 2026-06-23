#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# `aube create` wraps `dlx` with an npm-style `create-*` name mapping.
# The fixture registry has no `create-*` packages, so these tests assert
# on the "package not found" error, which reveals the translated name and
# proves both clap dispatch and the name mapping work end-to-end.

_write_pkg() {
	cat >package.json <<-'EOF'
		{ "name": "create-smoke", "version": "1.0.0", "private": true }
	EOF
}

@test "aube create prefixes unscoped template names with create-" {
	_write_pkg
	run aube create nonexistent-aube-smoke
	assert_failure
	assert_output --partial "create-nonexistent-aube-smoke"
}

@test "aube create leaves already-prefixed names alone" {
	_write_pkg
	run aube create create-nonexistent-aube-smoke
	assert_failure
	assert_output --partial "create-nonexistent-aube-smoke"
	refute_output --partial "create-create-nonexistent-aube-smoke"
}

@test "aube create maps scoped @scope/foo to @scope/create-foo" {
	_write_pkg
	run aube create @aube-fixture/nonexistent-smoke
	assert_failure
	assert_output --partial "@aube-fixture/create-nonexistent-smoke"
}

@test "aube create maps bare @scope to @scope/create" {
	_write_pkg
	run aube create @aube-fixture-nonexistent
	assert_failure
	assert_output --partial "@aube-fixture-nonexistent/create"
}

@test "aube create --help prints aube's help for the subcommand" {
	run aube create --help
	assert_success
	assert_output --partial "Scaffold a project"
	assert_output --partial "create-*"
}

@test "aube create with no args prints help" {
	run aube create
	assert_success
	assert_output --partial "Scaffold a project"
}

@test "aube create accepts an @version suffix on unscoped names" {
	_write_pkg
	# The version gets forwarded to dlx/install; we only assert the
	# translated package name since the resolver fails at name lookup
	# before it reports the requested version.
	run aube create nonexistent-aube-smoke@1.2.3
	assert_failure
	assert_output --partial "create-nonexistent-aube-smoke"
}
