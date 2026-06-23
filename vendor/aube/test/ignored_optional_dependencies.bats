#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install honors pnpm.ignoredOptionalDependencies for root deps" {
	cat >package.json <<'JSON'
{
  "name": "ignored-opt-root",
  "version": "1.0.0",
  "optionalDependencies": {
    "is-odd": "3.0.1"
  },
  "pnpm": {
    "ignoredOptionalDependencies": ["is-odd"]
  }
}
JSON
	run aube install
	assert_success

	# is-odd was listed as an optional dep but the pnpm ignore block
	# should have stripped it before resolution — neither the linked
	# entry nor the virtual store copy may exist.
	assert_file_not_exists node_modules/is-odd
	run bash -c "ls node_modules/.aube 2>/dev/null | grep '^is-odd' || true"
	assert_output ""

	# Lockfile records the setting so a subsequent install reuses it
	# and the drift check recognizes edits to the field.
	run grep -F "ignoredOptionalDependencies:" aube-lock.yaml
	assert_success
	run grep -F -- "- is-odd" aube-lock.yaml
	assert_success

	# is-odd must not appear as a locked package.
	run grep -F "is-odd@" aube-lock.yaml
	assert_failure
}

@test "aube install --frozen-lockfile stays fresh with mixed regular + ignored optional deps" {
	# Regression: the drift check used to scan every manifest optional
	# dep against the lockfile importer, but ignored optionals are
	# deliberately absent from the importer. A project that mixes a
	# real prod dep with an ignored optional would therefore report
	# `manifest adds <name>` on every subsequent install — perpetual
	# re-resolve in the default mode, hard failure under --frozen-lockfile.
	cat >package.json <<'JSON'
{
  "name": "ignored-opt-mixed",
  "version": "1.0.0",
  "dependencies": {
    "ansi-regex": "6.0.1"
  },
  "optionalDependencies": {
    "is-odd": "3.0.1"
  },
  "pnpm": {
    "ignoredOptionalDependencies": ["is-odd"]
  }
}
JSON
	run aube install
	assert_success
	run bash -c "ls node_modules/.aube | grep '^ansi-regex@' || true"
	assert_output --partial "ansi-regex@"
	run bash -c "ls node_modules/.aube 2>/dev/null | grep '^is-odd' || true"
	assert_output ""

	# Second install under --frozen-lockfile must succeed — any drift
	# would error out here.
	run aube install --frozen-lockfile
	assert_success
}

@test "aube install re-resolves when ignoredOptionalDependencies drifts" {
	cat >package.json <<'JSON'
{
  "name": "ignored-opt-drift",
  "version": "1.0.0",
  "optionalDependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install
	assert_success
	# Without an ignore block, is-odd gets resolved and materialized
	# into the virtual store.
	run bash -c "ls node_modules/.aube | grep '^is-odd@' || true"
	assert_output --partial "is-odd@"

	# Add is-odd to the ignore list and reinstall. Drift detection
	# should force a re-resolve that drops the package from the graph.
	cat >package.json <<'JSON'
{
  "name": "ignored-opt-drift",
  "version": "1.0.0",
  "optionalDependencies": {
    "is-odd": "3.0.1"
  },
  "pnpm": {
    "ignoredOptionalDependencies": ["is-odd"]
  }
}
JSON
	run aube install
	assert_success
	# After the drift-triggered re-resolve, the lockfile must no
	# longer list is-odd as a resolved package.
	run grep -F "is-odd@" aube-lock.yaml
	assert_failure
	run grep -F "ignoredOptionalDependencies:" aube-lock.yaml
	assert_success
}
