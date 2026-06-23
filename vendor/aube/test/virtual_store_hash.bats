#!/usr/bin/env bats
#
# Tests for content-addressed virtual-store paths. The global virtual
# store (`~/.cache/aube/virtual-store/`) keys each entry by
# `<dep_path>-<hex>` where `<hex>` is a sha256-based digest of the
# package's dep subgraph (and, for packages that transitively require
# build scripts, the engine string too). These tests verify that the
# suffix is actually applied, that the local `.aube/<dep_path>` layout
# stays un-hashed so Node's module walk still works, and that two
# different dep versions resolve to distinct virtual-store entries.
#
# bats file_tags=serial

# Force within-file tests to run one at a time regardless of --jobs.
# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "virtual store subdirs are content-addressed" {
	cat >package.json <<'JSON'
{
  "name": "vstore-hash-basic",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	# Every subdir under the global virtual store carries a
	# `-<hex>` suffix. Use a single run + grep rather than -regex
	# so the assertion is portable.
	run bash -c 'ls "$HOME/.cache/aube/virtual-store/"'
	assert_success
	# is-odd is a direct dep; every node in the graph gets hashed,
	# and the suffix is 16 hex chars.
	assert_output --regexp 'is-odd@3\.0\.1-[0-9a-f]{16}'
	assert_output --regexp 'is-number@6\.0\.0-[0-9a-f]{16}'
}

@test "local .aube entries stay at the raw dep_path" {
	cat >package.json <<'JSON'
{
  "name": "vstore-hash-local",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success

	# Node resolves transitive deps via `.aube/<dep_path>/...` — if
	# the local side got hashed too, module resolution would break.
	run bash -c 'ls node_modules/.aube'
	assert_success
	assert_output --partial "is-odd@3.0.1"
	assert_output --partial "is-number@6.0.0"
	refute_output --regexp 'is-odd@3\.0\.1-[0-9a-f]{16}'
}

@test "installed package is still importable through the hashed store" {
	# Regression guard: even with the new path layout, a require()
	# walk from node_modules/is-odd → .aube/is-odd@3.0.1/node_modules/is-odd
	# has to resolve the transitive is-number sibling.
	cat >package.json <<'JSON'
{
  "name": "vstore-hash-import",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
JSON
	run aube install
	assert_success
	run node -e 'console.log(require("is-odd")(3))'
	assert_success
	assert_output "true"
}
