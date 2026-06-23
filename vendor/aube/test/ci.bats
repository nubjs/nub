#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube ci installs from a committed lockfile" {
	_setup_basic_fixture
	run aube ci
	assert_success
	assert_file_exists node_modules/is-odd/package.json
	assert_file_exists aube-lock.yaml
}

@test "aube ci deletes existing node_modules first" {
	_setup_basic_fixture
	# Seed an "old" node_modules with a stale sentinel file
	mkdir -p node_modules
	touch node_modules/.stale-sentinel
	run aube ci
	assert_success
	# Sentinel should be gone — ci deletes node_modules before installing
	assert [ ! -e node_modules/.stale-sentinel ]
	# Fresh install artifacts should be in place
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube ci errors when no lockfile is present" {
	echo '{"name":"no-lockfile","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	run aube ci
	assert_failure
	assert_output --partial "no lockfile found and --frozen-lockfile is set"
}

@test "aube ci errors when lockfile drifts from package.json" {
	_setup_basic_fixture
	# Mutate package.json so the lockfile is stale
	node -e '
		const fs = require("fs");
		const pkg = JSON.parse(fs.readFileSync("package.json"));
		pkg.dependencies["is-odd"] = "^99.0.0";
		fs.writeFileSync("package.json", JSON.stringify(pkg, null, 2));
	'
	run aube ci
	assert_failure
	assert_output --partial "lockfile is out of date"
}

@test "aube ci --ignore-scripts accepts the flag" {
	_setup_basic_fixture
	run aube ci --ignore-scripts
	assert_success
}

@test "aube clean-install is an alias for aube ci" {
	_setup_basic_fixture
	run aube clean-install
	assert_success
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube ci removes a symlink node_modules without wiping its target" {
	# If node_modules is a symlink to an unrelated directory (rare but
	# legal), ci must unlink the symlink itself and NOT recursively delete
	# the target directory. remove_existing() in commands/mod.rs handles
	# this via a symlink check; this test guards against regressions where
	# a naive remove_dir_all would follow the symlink and wipe the target.
	_setup_basic_fixture
	mkdir -p "$TEST_TEMP_DIR/elsewhere"
	touch "$TEST_TEMP_DIR/elsewhere/must-survive.txt"
	ln -s "$TEST_TEMP_DIR/elsewhere" node_modules
	run aube ci
	assert_success
	# Target directory and its contents must still exist.
	assert_file_exists "$TEST_TEMP_DIR/elsewhere/must-survive.txt"
	# Fresh node_modules should be a real directory now, not the symlink.
	run test -L node_modules
	assert_failure
	assert_dir_exists node_modules
	assert_file_exists node_modules/is-odd/package.json
}
