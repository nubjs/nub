#!/usr/bin/env bats

# `shamefully-hoist` flattens every resolved package into the top-level
# `node_modules/`, mirroring npm's layout. By default transitive deps
# live under `.aube/.../node_modules/` and only direct deps get a
# top-level symlink; this test flips the knob and verifies a
# transitive dep (`is-number` pulled in via `is-odd`) becomes
# importable from the root.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "shamefully-hoist=false leaves transitives out of top-level node_modules" {
	cat >package.json <<'JSON'
{
  "name": "hoist-off",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd
	# is-number is a transitive dep only — must NOT be at the root.
	assert_not_exists node_modules/is-number
}

@test "shamefully-hoist=true promotes transitives to top-level node_modules" {
	cat >package.json <<'JSON'
{
  "name": "hoist-on",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	echo "shamefully-hoist=true" >.npmrc
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd
	assert_link_exists node_modules/is-number
}

@test "--shamefully-hoist CLI flag promotes transitives even without config" {
	cat >package.json <<'JSON'
{
  "name": "hoist-on-cli",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install --shamefully-hoist
	assert_success
	assert_link_exists node_modules/is-odd
	assert_link_exists node_modules/is-number
}

@test "--shamefully-hoist CLI flag overrides shamefully-hoist=false in .npmrc" {
	cat >package.json <<'JSON'
{
  "name": "hoist-cli-override",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	echo "shamefully-hoist=false" >.npmrc
	run aube install --shamefully-hoist
	assert_success
	assert_link_exists node_modules/is-odd
	assert_link_exists node_modules/is-number
}

@test "shamefullyHoist in pnpm-workspace.yaml promotes transitives to top-level node_modules" {
	cat >package.json <<'JSON'
{
  "name": "hoist-on-yaml",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
shamefullyHoist: true
YAML
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd
	assert_link_exists node_modules/is-number
}
