#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube licenses groups installed packages by license" {
	_setup_basic_fixture
	aube install

	run aube licenses
	assert_success
	assert_output --partial "MIT"
	assert_output --partial "is-odd@3.0.1"
	assert_output --partial "is-even@1.0.0"
}

@test "aube licenses ls is a compat alias for aube licenses" {
	_setup_basic_fixture
	aube install

	run aube licenses ls
	assert_success
	assert_output --partial "is-odd@3.0.1"
}

@test "aube licenses ls accepts flags after the subcommand (pnpm order)" {
	# Regression: when `ls` was a clap subcommand with no args of its own,
	# `aube licenses ls --json` failed with "unexpected argument" because
	# the flags belonged to the parent command. pnpm scripts put the flags
	# after the subcommand, so this invocation order must work.
	_setup_basic_fixture
	aube install

	run aube licenses ls --json
	assert_success
	assert_output --partial '"name": "is-odd"'
	assert_output --partial '"license": "MIT"'
}

@test "aube licenses --json emits a JSON array with name/version/license" {
	_setup_basic_fixture
	aube install

	run aube licenses --json
	assert_success
	assert_output --partial '"name": "is-odd"'
	assert_output --partial '"license": "MIT"'
	# No path key without --long.
	refute_output --partial '"path"'
}

@test "aube licenses --long includes the resolved on-disk path" {
	_setup_basic_fixture
	aube install

	run aube licenses --long --json
	assert_success
	assert_output --partial '"path"'
	assert_output --partial "/node_modules/.aube/is-odd@3.0.1/node_modules/is-odd"
	# Must be the package directory, not the package.json file inside it.
	refute_output --partial "package.json"
}

@test "aube licenses --long honors virtualStoreDir" {
	_setup_basic_fixture
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=node_modules/.custom-vs
	EOF
	aube install

	run aube licenses --long --json
	assert_success
	# Path must be under the configured virtual store, not the
	# default — and the reported file must still exist so the
	# resolver can read its license.
	assert_output --partial "/node_modules/.custom-vs/is-odd@3.0.1/node_modules/is-odd"
	refute_output --partial "/node_modules/.aube/"
}

@test "aube licenses --prod hides devDependencies" {
	cat >package.json <<'EOF'
{
  "name": "lic-prod",
  "version": "0.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "is-even": "^1.0.0" }
}
EOF
	aube install

	run aube licenses --prod
	assert_success
	assert_output --partial "is-odd@3.0.1"
	refute_output --partial "is-even"
}

@test "aube licenses --dev hides production dependencies" {
	cat >package.json <<'EOF'
{
  "name": "lic-dev",
  "version": "0.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "is-even": "^1.0.0" }
}
EOF
	aube install

	run aube licenses --dev
	assert_success
	assert_output --partial "is-even@1.0.0"
	# is-odd@3.0.1 is the prod-only direct dep; it must not appear under --dev.
	# (is-even transitively pulls in is-odd@0.1.2, which is expected.)
	refute_output --partial "is-odd@3.0.1"
}

@test "aube licenses resolves scoped packages (regression: dep_path encoding)" {
	# Regression for the virtual-store path bug: scoped deps live at
	# `.aube/@scope+name@version/node_modules/@scope/name/...` — the
	# outer `.aube/` entry is a single flat directory produced by
	# `dep_path_to_filename` (slashes flattened to `+`), and the
	# inner node_modules keeps the real scope path. Before the fix,
	# read_license opened the wrong path and every scoped package
	# came back as UNKNOWN.
	cat >package.json <<'EOF'
{
  "name": "lic-scoped",
  "version": "0.0.0",
  "dependencies": { "@babel/helper-validator-identifier": "7.22.20" }
}
EOF
	aube install

	run aube licenses
	assert_success
	assert_output --partial "@babel/helper-validator-identifier@7.22.20"
	refute_output --partial "UNKNOWN"
}

@test "aube licenses with no lockfile is a friendly no-op" {
	cat >package.json <<'EOF'
{ "name": "lic-empty", "version": "0.0.0" }
EOF

	run aube licenses
	assert_success
	assert_output --partial "No lockfile found"
}
