#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_write_pkg() {
	cat >package.json <<-EOF
		{
		  "name": "lifecycle-fixture",
		  "version": "1.0.0",
		  "scripts": $1
		}
	EOF
}

@test "aube start runs the start script" {
	_write_pkg '{"start":"echo started"}'
	run aube start
	assert_success
	assert_output --partial "started"
}

@test "aube stop runs the stop script" {
	_write_pkg '{"stop":"echo stopped"}'
	run aube stop
	assert_success
	assert_output --partial "stopped"
}

@test "aube test runs the test script" {
	_write_pkg '{"test":"echo tested"}'
	run aube test
	assert_success
	assert_output --partial "tested"
}

@test "aube t aliases test" {
	_write_pkg '{"test":"echo tested"}'
	run aube t
	assert_success
	assert_output --partial "tested"
}

@test "aube restart runs the restart script when defined" {
	_write_pkg '{"restart":"echo restarted","start":"echo SHOULD_NOT_RUN","stop":"echo SHOULD_NOT_RUN"}'
	run aube restart
	assert_success
	assert_output --partial "restarted"
	refute_output --partial "SHOULD_NOT_RUN"
}

@test "aube restart falls back to stop + start" {
	_write_pkg '{"stop":"echo stopping","start":"echo starting"}'
	run aube restart
	assert_success
	assert_output --partial "stopping"
	assert_output --partial "starting"
}

@test "aube restart succeeds silently with no lifecycle scripts" {
	_write_pkg '{}'
	run aube restart
	assert_success
	refute_output --partial "Auto-installing"
}

@test "aube restart tolerates missing stop script" {
	_write_pkg '{"start":"echo starting"}'
	run aube restart
	assert_success
	assert_output --partial "starting"
}

@test "aube start forwards trailing args" {
	_write_pkg '{"start":"echo hello"}'
	run aube start world
	assert_success
	assert_output --partial "hello world"
}

@test "aube start fails when no start script defined" {
	_write_pkg '{}'
	run aube start
	assert_failure
	assert_output --partial "script not found"
}

@test "aube install-test runs the test script and warns about redundancy" {
	_write_pkg '{"test":"echo tested"}'
	run aube install-test
	assert_success
	assert_output --partial "tested"
	assert_output --partial "redundant"
}

@test "aube it aliases install-test" {
	_write_pkg '{"test":"echo tested"}'
	run aube it
	assert_success
	assert_output --partial "tested"
}

@test "aube install-test fails fast when no test script is defined" {
	_write_pkg '{}'
	run aube install-test
	assert_failure
	assert_output --partial "script not found: test"
	# Script-existence check runs before install, so no node_modules/lockfile should appear.
	[ ! -d node_modules ]
	[ ! -f aube-lock.yaml ]
}
