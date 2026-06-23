#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "bare aube prints help instead of running install" {
	# pnpm parity: running `pnpm` with no command prints help and exits
	# 0. Aube used to silently run `install`; this test locks in the
	# pnpm-compatible behavior so a subdirectory without a package.json
	# (e.g. accidentally launched from $HOME) doesn't blow up with a
	# confusing "package.json not found".
	_setup_basic_fixture
	run aube
	assert_success
	assert_output --partial "Usage: aube"
	assert_output --partial "install"
	# Must NOT have installed anything.
	assert_not_exists node_modules
}

@test "aube install walks up to find package.json when run in a subdirectory" {
	_setup_basic_fixture
	mkdir -p docs
	cd docs
	run aube install
	assert_success
	# The install should have materialized into the project root, not
	# the current subdirectory.
	assert_dir_exists "$TEST_TEMP_DIR/node_modules"
	assert_not_exists node_modules
}

@test "aube run walks up to find package.json when run in a subdirectory" {
	_setup_basic_fixture
	mkdir -p docs
	cd docs
	run aube run hello
	assert_success
	assert_output --partial "hello from aube!"
}

@test "implicit aube script walks up to find package.json when run in a subdirectory" {
	_setup_basic_fixture
	mkdir -p docs
	cd docs
	run aube hello
	assert_success
	assert_output --partial "hello from aube!"
}

@test "aube root and bin print project paths when run in a subdirectory" {
	_setup_basic_fixture
	mkdir -p docs
	cd docs
	physical_root="$(cd .. && pwd -P)"

	run aube root
	assert_success
	assert_output "$physical_root/node_modules"

	run aube bin
	assert_success
	assert_output "$physical_root/node_modules/.bin"
}

@test "aube list walks up to read the project manifest and lockfile" {
	_setup_basic_fixture
	mkdir -p docs
	cd docs
	run aube list --depth 0
	assert_success
	assert_output --partial "aube-test-basic@1.0.0"
	assert_output --partial "is-odd"
	assert_output --partial "is-even"
}

@test "aube version walks up before editing package.json" {
	_setup_basic_fixture
	mkdir -p docs
	cd docs
	run aube version patch --no-git-tag-version
	assert_success
	assert_output "v1.0.1"
	assert_file_contains "$TEST_TEMP_DIR/package.json" '"version": "1.0.1"'
	assert_not_exists package.json
}

@test "aube install errors clearly when no package.json exists in any ancestor" {
	# Ensure we're in a directory tree with no package.json above us.
	mkdir -p empty/sub
	cd empty/sub
	run aube install
	assert_failure
	assert_output --partial "no package.json or workspace yaml"
}

@test "aube install generates aube-lock.yaml when no lockfile exists" {
	cp "$PROJECT_ROOT/fixtures/basic/package.json" .
	# Explicitly do NOT copy any lockfile — exercise the fresh-resolve
	# write path.
	assert_not_exists aube-lock.yaml
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
}
