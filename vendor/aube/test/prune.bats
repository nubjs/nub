#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube prune --help" {
	run aube prune --help
	assert_success
	assert_output --partial "Remove extraneous packages"
	assert_output --partial "--prod"
	assert_output --partial "--no-optional"
}

@test "aube prune errors without a lockfile" {
	cat >package.json <<'EOF'
{"name":"empty","version":"1.0.0"}
EOF

	run aube prune
	assert_failure
	assert_output --partial "lockfile"
}

@test "aube prune on a clean install is a no-op" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube prune
	assert_success
	assert_output --partial "Nothing to prune"
}

@test "aube prune removes orphaned .aube entries" {
	_setup_basic_fixture
	run aube install
	assert_success

	# Simulate a stale entry left behind by a previous install.
	mkdir -p node_modules/.aube/orphan-pkg@1.0.0
	run test -d node_modules/.aube/orphan-pkg@1.0.0
	assert_success

	run aube prune
	assert_success
	assert_output --partial "1 from .aube"

	run test -e node_modules/.aube/orphan-pkg@1.0.0
	assert_failure

	# Real deps should still be present
	run test -L node_modules/is-odd
	assert_success
}

@test "aube prune honors virtualStoreDir" {
	_setup_basic_fixture
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=node_modules/.custom-vs
	EOF
	run aube install
	assert_success
	assert_dir_exists node_modules/.custom-vs

	# Plant an orphan at the configured virtual store.
	mkdir -p node_modules/.custom-vs/orphan-pkg@1.0.0
	run aube prune
	assert_success
	assert_output --partial "1 from .aube"
	run test -e node_modules/.custom-vs/orphan-pkg@1.0.0
	assert_failure

	# Real deps survive.
	run test -L node_modules/is-odd
	assert_success
}

@test "aube prune preserves non-dotfile virtualStoreDir under modulesDir" {
	# Regression: prune_top_level's dotfile short-circuit didn't
	# cover a non-dotfile virtualStoreDir like `node_modules/vstore`.
	# Without the preserve_leaf guard, prune would delete the entire
	# virtual store because `vstore` isn't in the allowed dep names.
	_setup_basic_fixture
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=node_modules/vstore
	EOF
	run aube install
	assert_success
	assert_dir_exists node_modules/vstore

	run aube prune
	assert_success

	# Virtual store still there, real deps still there.
	assert_dir_exists node_modules/vstore
	run ls node_modules/vstore
	assert_success
	assert_output --partial 'is-odd'
	run test -L node_modules/is-odd
	assert_success
}

@test "aube prune removes orphan scoped .aube entries and cleans empty scope dir" {
	_setup_basic_fixture
	run aube install
	assert_success

	mkdir -p "node_modules/.aube/@myorg/widgets@1.0.0"

	run aube prune
	assert_success
	assert_output --partial "1 from .aube"

	run test -e "node_modules/.aube/@myorg/widgets@1.0.0"
	assert_failure
	# Empty scope dir should be cleaned up
	run test -e "node_modules/.aube/@myorg"
	assert_failure
}

@test "aube prune removes orphan top-level symlinks" {
	_setup_basic_fixture
	run aube install
	assert_success

	# Create a rogue top-level symlink pointing somewhere.
	ln -s "$TEST_TEMP_DIR" node_modules/fake-pkg

	run aube prune
	assert_success
	assert_output --partial "1 top-level"

	run test -e node_modules/fake-pkg
	assert_failure
	run test -L node_modules/is-odd
	assert_success
}

@test "aube prune cleans dangling .bin symlinks" {
	_setup_basic_fixture
	run aube install
	assert_success

	mkdir -p node_modules/.bin
	# Symlink pointing at a nonexistent file
	ln -s /nonexistent/bogus-bin node_modules/.bin/bogus

	run aube prune
	assert_success
	assert_output --partial "1 dangling .bin"

	run test -e node_modules/.bin/bogus
	assert_failure
}

@test "aube prune is idempotent" {
	_setup_basic_fixture
	run aube install
	assert_success

	mkdir -p node_modules/.aube/orphan@1.0.0

	run aube prune
	assert_success

	run aube prune
	assert_success
	assert_output --partial "Nothing to prune"
}

@test "aube prune --prod removes devDependencies after install" {
	# Craft a project with a dev dep, install it, then prune --prod.
	cat >package.json <<'EOF'
{
  "name": "aube-test-prune-prod",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "kind-of": "6.0.3" }
}
EOF

	run aube install
	assert_success

	run test -L node_modules/kind-of
	assert_success
	run test -L node_modules/is-odd
	assert_success

	run aube prune --prod
	assert_success

	# Dev dep should be gone from top-level
	run test -e node_modules/kind-of
	assert_failure
	# Dev dep's .aube entry should also be gone
	run bash -c "ls node_modules/.aube/ | grep -E '^kind-of@'"
	assert_failure

	# Prod dep should remain
	run test -L node_modules/is-odd
	assert_success
}

@test "aube install --no-optional excludes optional deps" {
	cat >package.json <<'EOF'
{
  "name": "aube-test-no-optional",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "optionalDependencies": { "kind-of": "6.0.3" }
}
EOF

	run aube install --no-optional
	assert_success

	# Prod dep should be present
	run test -L node_modules/is-odd
	assert_success
	# Optional dep should be absent from top-level
	run test -e node_modules/kind-of
	assert_failure
	# And absent from .aube/ too
	run bash -c "ls node_modules/.aube/ 2>/dev/null | grep -E '^kind-of@'"
	assert_failure
}

@test "aube ci --no-optional is accepted and excludes optional deps" {
	cat >package.json <<'EOF'
{
  "name": "aube-test-ci-no-optional",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" },
  "optionalDependencies": { "kind-of": "6.0.3" }
}
EOF

	# First produce a lockfile (ci requires one).
	run aube install
	assert_success

	run aube ci --no-optional
	assert_success

	run test -L node_modules/is-odd
	assert_success
	run test -e node_modules/kind-of
	assert_failure
}
