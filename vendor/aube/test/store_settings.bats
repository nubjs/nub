#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# `strictStorePkgContentCheck=true` (the pnpm default) cross-checks
# every imported registry tarball's `package.json` against the
# (name, version) the resolver requested. The
# `aube-test-content-liar` fixture under
# `test/registry/storage/aube-test-content-liar/` ships a packument
# advertising name `aube-test-content-liar` whose tarball's
# `package.json` actually declares
# `name: "aube-test-content-impostor"` — exactly the
# registry-substitution shape the check is designed to catch.

@test "strictStorePkgContentCheck=true (default) rejects mismatched manifest" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-strict-content-default",
		  "version": "1.0.0",
		  "dependencies": { "aube-test-content-liar": "1.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_failure
	# Caller's `{name}@{version}: ` prefix supplies the expected
	# coordinate; the error variant supplies the actual one. miette
	# word-wraps the diagnostic so each piece may end up on its own
	# line — match independently.
	assert_output --partial 'package.json content mismatch'
	assert_output --partial 'aube-test-content-liar@1.0.0'
	assert_output --partial 'declares aube-test-content-impostor@1.0.0'
	# The mismatched package must not be linked into node_modules.
	run test -e node_modules/aube-test-content-liar
	assert_failure
}

@test "strictStorePkgContentCheck=false accepts mismatched manifest" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-strict-content-off",
		  "version": "1.0.0",
		  "dependencies": { "aube-test-content-liar": "1.0.0" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		strict-store-pkg-content-check=false
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/aube-test-content-liar
}

# `useRunningStoreServer` describes a pnpm feature aube doesn't have.
# We accept the value (so a `.npmrc` ported from a pnpm store-server
# setup keeps working) but emit a one-line warning at install start so
# the user knows aube isn't honoring the strict semantic.

@test "useRunningStoreServer=true emits a warning and does not fail" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-store-server-warn",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		use-running-store-server=true
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_output --partial 'aube has no store server'
	assert_output --partial 'useRunningStoreServer=true is accepted but has no effect'
	assert_dir_exists node_modules/is-odd
}

@test "useRunningStoreServer=false (default) is silent" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-store-server-default",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	refute_output --partial 'aube has no store server'
}

# `dlxCacheMaxAge` is parsed and validated through the same resolver
# path the rest of the install uses, but `aube dlx` currently installs
# into a fresh `tempfile::TempDir` per invocation so there's nothing
# to evict. Setting a custom value must not break the install.

@test "dlxCacheMaxAge=60 is accepted by aube install" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-dlx-cache-age",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		dlx-cache-max-age=60
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
}
