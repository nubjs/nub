#!/usr/bin/env bash
# BATS suite-level setup: starts Verdaccio before any tests run.

setup_suite() {
	local test_dir
	test_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
	# shellcheck disable=SC1091
	source "$test_dir/registry/start.bash"
	start_registry
	# Export so all test files inherit this
	export AUBE_TEST_REGISTRY
}

teardown_suite() {
	stop_registry
}
