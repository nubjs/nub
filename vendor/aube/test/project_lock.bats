#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "AUBE_NO_LOCK env var bypasses project lock" {
	_setup_basic_fixture
	AUBE_NO_LOCK=1 run aube install
	assert_success
	assert_dir_exists node_modules
}

@test ".npmrc aubeNoLock=true bypasses project lock" {
	# Exercises the new `.npmrc` source for the `aubeNoLock` setting —
	# the env-var alias is the historical surface, but projects can now
	# opt out from config without relying on a shell export.
	_setup_basic_fixture
	echo "aubeNoLock=true" >.npmrc
	run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "aube-workspace.yaml aubeNoLock bypasses project lock" {
	# Exercises the workspace-yaml source. A workspace monorepo can
	# declare the bypass once and have every importer inherit it.
	_setup_basic_fixture
	cat >aube-workspace.yaml <<-EOF
		packages: []
		aubeNoLock: true
	EOF
	run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "aube install runs successfully with the project lock enabled" {
	_setup_basic_fixture
	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "concurrent aube install invocations complete without corruption" {
	_setup_basic_fixture

	local log1="$TEST_TEMP_DIR/install1.log"
	local log2="$TEST_TEMP_DIR/install2.log"

	aube install >"$log1" 2>&1 &
	local pid1=$!
	aube install >"$log2" 2>&1 &
	local pid2=$!

	wait "$pid1"
	local rc1=$?
	wait "$pid2"
	local rc2=$?

	if [ "$rc1" -ne 0 ]; then
		echo "first install failed (rc=$rc1):"
		cat "$log1"
		return 1
	fi
	if [ "$rc2" -ne 0 ]; then
		echo "second install failed (rc=$rc2):"
		cat "$log2"
		return 1
	fi

	# node_modules must be intact after both finish — this is the
	# property the lock actually protects.
	assert_dir_exists node_modules/is-odd
	assert_dir_exists node_modules/is-even
}
