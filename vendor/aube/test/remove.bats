#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube remove: removes a package" {
	cat >package.json <<'EOF'
{
  "name": "test-remove",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1",
    "is-even": "^1.0.0"
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	assert_file_exists node_modules/is-even/index.js

	run aube remove is-odd
	assert_success

	# package.json should no longer have is-odd
	run cat package.json
	refute_output --partial '"is-odd"'
	assert_output --partial '"is-even"'

	# node_modules should still have is-even but not is-odd as a top-level dep
	assert_file_exists node_modules/is-even/index.js
}

@test "aube remove: preserves package.json top-level key order" {
	cat >package.json <<'EOF'
{
  "name": "test-remove-order",
  "version": "0.0.0",
  "license": "MIT",
  "scripts": {
    "test": "echo test"
  },
  "dependencies": {
    "is-even": "^1.0.0",
    "is-odd": "^3.0.1"
  }
}
EOF

	run aube install
	assert_success

	run aube remove is-odd
	assert_success

	run node -e 'console.log(Object.keys(require("./package.json")).join(","))'
	assert_success
	assert_output 'name,version,license,scripts,dependencies'
}

@test "aube remove: errors on unknown package" {
	cat >package.json <<'EOF'
{
  "name": "test-remove-unknown",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	run aube remove nonexistent
	assert_failure
	assert_output --partial "not a dependency"
}

@test "aube remove: removes dev dependency" {
	cat >package.json <<'EOF'
{
  "name": "test-remove-dev",
  "version": "0.0.0",
  "dependencies": {},
  "devDependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	run aube install
	assert_success

	run aube remove is-odd
	assert_success

	run cat package.json
	refute_output --partial '"is-odd"'
}

@test "aube remove --save-dev only removes from devDependencies" {
	cat >package.json <<'EOF'
{
  "name": "test-remove-save-dev",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  },
  "devDependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	run aube remove --save-dev is-odd
	assert_success

	run node -e 'const p=require("./package.json"); if (!p.dependencies["is-odd"]) process.exit(1); if (p.devDependencies && p.devDependencies["is-odd"]) process.exit(2)'
	assert_success
}

@test "aube remove: removes multiple packages" {
	cat >package.json <<'EOF'
{
  "name": "test-remove-multi",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1",
    "is-even": "^1.0.0"
  }
}
EOF

	run aube install
	assert_success

	run aube remove is-odd is-even
	assert_success

	run cat package.json
	refute_output --partial '"is-odd"'
	refute_output --partial '"is-even"'
}

@test "aube remove --filter: keeps a surviving workspace:* dep resolvable" {
	# Regression: a filtered remove re-resolves the target package, and a
	# surviving `workspace:*` dependency must resolve to its local workspace
	# sibling — not be looked up on the registry, where it would fail with
	# ERR_AUBE_NO_MATCHING_VERSION. @test/app declares both `is-even`
	# (registry) and `@test/lib` (workspace:*); removing the former must leave
	# the latter intact and the lockfile updated in lockstep.
	cp -r "$PROJECT_ROOT/fixtures/workspace/"* .

	run aube install
	assert_success

	run aube remove is-even --filter @test/app
	assert_success
	refute_output --partial 'NO_MATCHING_VERSION'

	run cat packages/app/package.json
	refute_output --partial '"is-even"'
	assert_output --partial 'workspace:*'

	# Lockfile updated atomically: is-even gone, no stale entry left behind.
	run cat aube-lock.yaml
	refute_output --partial 'is-even'
}
