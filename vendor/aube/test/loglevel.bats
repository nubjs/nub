#!/usr/bin/env bats

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# `aube config set` unconditionally `eprintln!`s a confirmation line, so
# we use it as the probe for --silent coverage. BATS tests run with an
# isolated HOME, so the writes are sandboxed.

@test "aube config set emits stderr output by default (sanity probe)" {
	run --separate-stderr aube config set probe-key probe-value
	assert_success
	# If this ever goes empty, the --silent assertions below become
	# vacuous and need to be pointed at a different probe command.
	[ -n "$stderr" ]
}

@test "aube --silent config set produces no stderr output" {
	run --separate-stderr aube --silent config set probe-key probe-value
	assert_success
	[ -z "$stderr" ]
	[ -z "$output" ]
}

@test "aube --loglevel silent config set produces no stderr output" {
	run --separate-stderr aube --loglevel silent config set probe-key probe-value
	assert_success
	[ -z "$stderr" ]
	[ -z "$output" ]
}

@test "aube --silent still surfaces runtime errors to stderr" {
	# Must be a runtime error that fails *after* the SilentStderrGuard
	# is installed — clap parse errors happen before the guard exists
	# and would pass this test vacuously. `install` in an empty dir
	# fails at the "read package.json" step, deep inside the command
	# body, so it exercises the guard's Drop restore path.
	run --separate-stderr aube --silent install
	assert_failure
	[ -n "$stderr" ]
	[[ "$stderr" == *"package.json"* ]]
}

@test "aube --silent passes child process stderr through" {
	# `pnpm --loglevel silent` suppresses pnpm's own logging but leaves
	# script output intact. The SilentStderrGuard redirects fd 2 at the
	# OS level, so without the `child_stderr()` plumbing child
	# processes would inherit /dev/null. This test runs a script that
	# writes to stderr and asserts aube --silent still surfaces it.
	mkdir -p silent-child
	cd silent-child
	cat >package.json <<-'JSON'
		{
		  "name": "silent-child",
		  "version": "1.0.0",
		  "scripts": {
		    "say": "echo hello-from-script 1>&2"
		  }
		}
	JSON
	run --separate-stderr aube --silent run say
	assert_success
	[[ "$stderr" == *"hello-from-script"* ]]
}

@test "aube --loglevel debug enables debug logging" {
	_setup_basic_fixture
	run --separate-stderr aube --loglevel debug install
	assert_success
	# Match tracing's DEBUG level (preceded by space or ANSI escape), not
	# the `-DEBUG` suffix appended to the version string on debug builds.
	[[ "$stderr" =~ [^-]DEBUG ]]
}

@test "aube --loglevel rejects invalid level" {
	run aube --loglevel bogus install
	assert_failure
}

@test "aube --reporter=silent produces no stderr output" {
	run --separate-stderr aube --reporter=silent config set probe-key probe-value
	assert_success
	[ -z "$stderr" ]
	[ -z "$output" ]
}

@test "aube --reporter=silent still surfaces runtime errors to stderr" {
	# Mirrors the `--silent` error-surfacing test: both flags feed into
	# `effective_level = LogLevel::Silent`, so a regression that split
	# the two code paths should be caught here.
	run --separate-stderr aube --reporter=silent install
	assert_failure
	[ -n "$stderr" ]
	[[ "$stderr" == *"package.json"* ]]
}

@test "aube --reporter=ndjson emits JSON log events on stderr" {
	_setup_basic_fixture
	# Pair with --loglevel debug to drive some log volume through the
	# fmt layer. The assertion is about encoding, not volume: every
	# non-empty stderr line must be a JSON object, not the default
	# text layout.
	run --separate-stderr aube --loglevel debug --reporter=ndjson install
	assert_success
	first_line=$(printf '%s\n' "$stderr" | grep -v '^$' | head -n1)
	[ -n "$first_line" ]
	[[ "$first_line" == \{* ]]
	[[ "$first_line" == *\"level\"* ]]
}

@test "aube --reporter rejects invalid value" {
	run aube --reporter=bogus install
	assert_failure
}
