#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube why --help" {
	run aube why --help
	assert_success
	assert_output --partial "reverse dependency"
}

@test "aube why without a lockfile prints a friendly message" {
	cat >package.json <<'EOF'
{"name":"empty","version":"1.0.0"}
EOF

	run aube why some-pkg
	assert_success
	assert_output --partial "No lockfile found"
}

@test "aube why on a direct dep prints a single-frame chain" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why is-odd
	assert_success
	assert_output --partial "is-odd 3.0.1"
	# Should also match is-odd 0.1.2 which is a transitive dep of is-even
	assert_output --partial "is-odd 0.1.2"
}

@test "aube why on a transitive dep prints the full chain" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why is-number
	assert_success
	assert_output --partial "dependencies:"
	# is-number reached via is-odd@3.0.1
	assert_output --partial "is-odd 3.0.1"
	assert_output --partial "is-number 6.0.0"
	# And via is-even → is-odd@0.1.2
	assert_output --partial "is-even 1.0.0"
	assert_output --partial "is-number 3.0.0"
}

@test "aube why prints 'not in the dependency graph' for missing pkg" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why totally-not-a-real-package
	assert_success
	assert_output --partial "not in the dependency graph"
}

@test "aube why --json emits a JSON array" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why is-number --json
	assert_success
	# Must start with an opening bracket and contain chain objects
	assert_output --partial "["
	assert_output --partial "\"chain\""
	assert_output --partial "\"name\""
	assert_output --partial "\"version\""
}

@test "aube why --json outputs an empty array when not found" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why totally-not-real --json
	assert_success
	assert_output "[]"
}

@test "aube why --parseable produces tab-separated chains" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why is-number --parseable
	assert_success
	# Each line should contain tab characters (we just check for a literal tab)
	[[ "$output" == *$'\t'* ]]
	assert_output --partial "is-number@"
}

@test "aube why --parseable emits empty output when not found" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why totally-not-real --parseable
	assert_success
	# Must be empty — no human-readable fallback that would break
	# downstream tab-split parsers.
	assert_output ""
}

@test "aube why --long appends the store path" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why is-number --long
	assert_success
	assert_output --partial "(./node_modules/.aube/"
}

@test "aube why --long honors virtualStoreDir override" {
	# Regression: `why --long` hardcoded `./node_modules/.aube/` in the
	# rendered chain, masking the real location when the user had set
	# `virtualStoreDir` to a custom path.
	_setup_basic_fixture
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=node_modules/vstore
	EOF
	run aube install
	assert_success

	run aube why is-number --long
	assert_success
	assert_output --partial "(./node_modules/vstore/"
	refute_output --partial "(./node_modules/.aube/"
}

@test "aube why --long --parseable appends dep_path to each frame" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube why is-number --long --parseable
	assert_success
	# Each frame cell becomes `name@version|dep_path`. For is-number we
	# should see `is-number@6.0.0|is-number@6.0.0` in the output.
	assert_output --partial "is-number@6.0.0|is-number@6.0.0"
}

@test "aube why --dev restricts to devDependencies roots" {
	# Craft a fixture with kind-of as a dev dep
	cat >package.json <<'EOF'
{
  "name": "aube-test-why-dev",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "kind-of": "6.0.3" }
}
EOF
	run aube install
	assert_success

	# kind-of is a dev dep; --dev should still find it
	run aube why kind-of --dev
	assert_success
	assert_output --partial "kind-of 6.0.3"
	assert_output --partial "devDependencies"

	# is-number lives under the prod dep is-odd; --dev should NOT find it
	run aube why is-number --dev
	assert_success
	assert_output --partial "not in the dependency graph"
}

@test "aube why --prod restricts to production-rooted chains" {
	cat >package.json <<'EOF'
{
  "name": "aube-test-why-prod",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "kind-of": "6.0.3" }
}
EOF
	run aube install
	assert_success

	# kind-of is a dev dep; --prod should NOT find it
	run aube why kind-of --prod
	assert_success
	assert_output --partial "not in the dependency graph"

	# is-number is reached via the prod dep is-odd; --prod should find it
	run aube why is-number --prod
	assert_success
	assert_output --partial "is-number"
}

@test "aube why --filter uses workspace root importer paths from a package subdirectory" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/lib-a/docs packages/lib-b
	cat >packages/lib-a/package.json <<'EOF'
{
  "name": "@scope/lib-a",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" }
}
EOF
	cat >packages/lib-b/package.json <<'EOF'
{
  "name": "@scope/lib-b",
  "version": "1.0.0",
  "dependencies": { "is-even": "^1.0.0" }
}
EOF

	run aube install
	assert_success

	cd packages/lib-a/docs
	run aube -F @scope/lib-a why is-number --parseable
	assert_success
	assert_output --partial "packages/lib-a"
	refute_output --partial "packages/lib-b"
}

@test "aube why walks up to the workspace root from a subpackage cwd" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/lib-a
	cat >packages/lib-a/package.json <<'EOF'
{
  "name": "@scope/lib-a",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" }
}
EOF

	run aube install
	assert_success

	cd packages/lib-a
	run aube why is-number
	assert_success
	refute_output --partial "No lockfile found"
	assert_output --partial "is-number"
}
