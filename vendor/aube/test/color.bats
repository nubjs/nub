#!/usr/bin/env bats

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# The cleanest observable effect of --color / --no-color is the env vars
# propagated to child processes: aube translates the flags into
# NO_COLOR / FORCE_COLOR / CLICOLOR_FORCE before anything else reads
# them, and those inherit into `run` / `exec` / lifecycle scripts. A
# small `run` script that echoes the env vars is a reliable probe.

_make_env_probe_project() {
	mkdir -p color-probe
	cd color-probe || return
	cat >package.json <<-'JSON'
		{
		  "name": "color-probe",
		  "version": "1.0.0",
		  "scripts": {
		    "probe": "node -e \"console.log('NO_COLOR=' + (process.env.NO_COLOR || '')); console.log('FORCE_COLOR=' + (process.env.FORCE_COLOR || '')); console.log('CLICOLOR_FORCE=' + (process.env.CLICOLOR_FORCE || ''))\""
		  }
		}
	JSON
}

@test "aube --no-color sets NO_COLOR for child processes" {
	_make_env_probe_project
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE
	run aube --no-color run probe
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
	[[ "$output" == *"FORCE_COLOR="$'\n'* || "$output" == *"FORCE_COLOR=" ]]
	[[ "$output" == *"CLICOLOR_FORCE="$'\n'* || "$output" == *"CLICOLOR_FORCE=" ]]
}

@test "aube --color sets FORCE_COLOR / CLICOLOR_FORCE for child processes" {
	_make_env_probe_project
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE
	run aube --color run probe
	assert_success
	[[ "$output" == *"FORCE_COLOR=1"* ]]
	[[ "$output" == *"CLICOLOR_FORCE=1"* ]]
}

@test "aube --color overrides inherited NO_COLOR" {
	_make_env_probe_project
	NO_COLOR=1 run aube --color run probe
	assert_success
	[[ "$output" == *"FORCE_COLOR=1"* ]]
	# NO_COLOR should have been removed before the child was spawned.
	[[ "$output" != *"NO_COLOR=1"* ]]
}

@test "aube --no-color overrides inherited FORCE_COLOR" {
	_make_env_probe_project
	FORCE_COLOR=1 run aube --no-color run probe
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
	[[ "$output" != *"FORCE_COLOR=1"* ]]
}

@test "aube --color and --no-color are mutually exclusive" {
	run aube --color --no-color install
	assert_failure
}
