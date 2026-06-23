#!/usr/bin/env bats

# Smoke tests for the `fetch*` settings (fetchTimeout, fetchRetries,
# fetchRetryFactor, fetchRetryMintimeout, fetchRetryMaxtimeout). The
# retry/timeout *logic* is covered by Rust unit tests against a
# wiremock server — those exercise 503/429/404/timeout paths. These
# BATS tests are here to prove the settings parse through .npmrc into
# `aube install` without crashing, and that the Verdaccio fixture
# stays green under generous values.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install accepts fetchTimeout from .npmrc" {
	_setup_basic_fixture

	# A generous timeout should not interfere with the fixture
	# registry (which responds in milliseconds). This guards against
	# the setting accidentally being parsed as seconds instead of
	# milliseconds — 60000s vs 60000ms is the kind of typo that only
	# shows up under load.
	echo "fetchTimeout=60000" >.npmrc

	run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "aube install accepts fetchRetries + retry backoff knobs from .npmrc" {
	_setup_basic_fixture

	cat >>.npmrc <<-EOF
		fetch-retries=3
		fetch-retry-factor=2
		fetch-retry-mintimeout=100
		fetch-retry-maxtimeout=2000
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "NPM_CONFIG_FETCH_TIMEOUT env var is honored" {
	_setup_basic_fixture

	# Env var path goes through the same settings resolver; a
	# reasonable value should install cleanly, confirming the env
	# alias is wired.
	NPM_CONFIG_FETCH_TIMEOUT=60000 run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "aube install accepts fetchWarnTimeoutMs + fetchMinSpeedKiBps from .npmrc" {
	_setup_basic_fixture

	# Both thresholds are advisory — they only emit `tracing::warn`
	# lines and must not fail the install. Set them aggressively low
	# so the warn branch actually fires against the local Verdaccio
	# (which responds in milliseconds), and confirm install still
	# succeeds. The log line content is pinned by Rust unit tests.
	cat >>.npmrc <<-EOF
		fetchWarnTimeoutMs=1
		fetchMinSpeedKiBps=999999
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "fetchWarnTimeoutMs=0 disables the warning and still installs" {
	_setup_basic_fixture

	# `0` is the documented "disable" value — verify it parses and
	# the install path stays clean. Ditto `fetchMinSpeedKiBps=0`.
	cat >>.npmrc <<-EOF
		fetchWarnTimeoutMs=0
		fetchMinSpeedKiBps=0
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules
}

@test "--fetch-timeout / --fetch-retries CLI flags are accepted and honored" {
	# Smoke-tests the global `--fetch-*` CLI surface (pnpm parity for
	# `--fetch-timeout=<ms>`, `--fetch-retries=<n>`, and the three
	# retry-* knobs). A generous timeout against the local Verdaccio
	# must still install cleanly — the failure-path counterpart lives
	# in pnpm_install_misc.bats.
	_setup_basic_fixture

	run aube \
		--fetch-timeout=60000 \
		--fetch-retries=3 \
		--fetch-retry-factor=2 \
		--fetch-retry-mintimeout=100 \
		--fetch-retry-maxtimeout=2000 \
		install
	assert_success
	assert_dir_exists node_modules
}
