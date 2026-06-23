#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube clean --help" {
	run aube clean --help
	assert_success
	assert_output --partial "Remove"
	assert_output --partial "node_modules"
	assert_output --partial "--lockfile"
}

@test "aube clean on empty dir is a no-op" {
	cat >package.json <<'EOF'
{"name":"empty","version":"1.0.0"}
EOF

	run aube clean
	assert_success
	assert_output --partial "Nothing to clean"
}

@test "aube clean removes node_modules from a single-project repo" {
	cat >package.json <<'EOF'
{"name":"demo","version":"1.0.0"}
EOF
	mkdir -p node_modules/foo
	echo '{}' >node_modules/foo/package.json

	run aube clean
	assert_success
	assert_output --partial "Removed 1 node_modules"
	assert [ ! -e node_modules ]
}

@test "aube clean --lockfile also removes the root lockfile(s)" {
	cat >package.json <<'EOF'
{"name":"demo","version":"1.0.0"}
EOF
	mkdir -p node_modules
	: >aube-lock.yaml
	: >pnpm-lock.yaml
	: >package-lock.json
	: >bun.lock

	run aube clean --lockfile
	assert_success
	assert_output --partial "4 lockfiles"
	assert [ ! -e aube-lock.yaml ]
	assert [ ! -e pnpm-lock.yaml ]
	assert [ ! -e package-lock.json ]
	assert [ ! -e bun.lock ]
}

@test "aube clean walks workspace packages" {
	cat >package.json <<'EOF'
{"name":"root","version":"1.0.0"}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - 'packages/*'
EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"1.0.0"}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"1.0.0"}
EOF
	mkdir -p node_modules packages/a/node_modules packages/b/node_modules

	run aube clean
	assert_success
	assert_output --partial "Removed 3 node_modules"
	assert [ ! -e node_modules ]
	assert [ ! -e packages/a/node_modules ]
	assert [ ! -e packages/b/node_modules ]
}

@test "aube clean defers to a user-defined clean script" {
	cat >package.json <<'EOF'
{
  "name": "demo",
  "version": "1.0.0",
  "scripts": {
    "clean": "echo custom-clean-ran"
  }
}
EOF
	mkdir -p node_modules/foo

	run aube clean
	assert_success
	assert_output --partial "custom-clean-ran"
	# User script wins: the built-in removal is skipped.
	assert [ -e node_modules/foo ]
}

@test "aube clean --lockfile warns when a clean script is defined" {
	cat >package.json <<'EOF'
{
  "name": "demo",
  "version": "1.0.0",
  "scripts": {
    "clean": "echo custom-clean-ran"
  }
}
EOF
	: >aube-lock.yaml

	run aube clean --lockfile
	assert_success
	assert_output --partial "--lockfile ignored"
	assert_output --partial "custom-clean-ran"
	# Script took over, so the lockfile we would have removed stays.
	assert [ -e aube-lock.yaml ]
}

@test "aube clean --lockfile omits zero-lockfile noise when nothing to remove" {
	cat >package.json <<'EOF'
{"name":"demo","version":"1.0.0"}
EOF
	mkdir -p node_modules

	run aube clean --lockfile
	assert_success
	assert_output --partial "Removed 1 node_modules directory"
	refute_output --partial "0 lockfiles"
}

@test "aube purge is an alias for clean" {
	cat >package.json <<'EOF'
{"name":"demo","version":"1.0.0"}
EOF
	mkdir -p node_modules

	run aube purge
	assert_success
	assert_output --partial "Removed 1 node_modules"
	assert [ ! -e node_modules ]
}

@test "aube purge defers to a user-defined purge script" {
	cat >package.json <<'EOF'
{
  "name": "demo",
  "version": "1.0.0",
  "scripts": {
    "purge": "echo custom-purge-ran"
  }
}
EOF
	mkdir -p node_modules/foo

	run aube purge
	assert_success
	assert_output --partial "custom-purge-ran"
	assert [ -e node_modules/foo ]
}
