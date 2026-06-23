#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

teardown() {
	_common_teardown
}

# The Verdaccio fixture registry serves per-version tarballs at
# `<registry>/<name>/-/<name>-<version>.tgz`, so we can exercise the
# remote-tarball-URL code path without spinning up a second HTTP
# server: just point `dependencies` at that URL directly.

@test "aube install handles remote tarball URL dep" {
	url="${AUBE_TEST_REGISTRY%/}/is-odd/-/is-odd-3.0.1.tgz"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"is-odd":"$url"}}
EOF

	run aube install
	assert_success

	assert_file_exists node_modules/is-odd/package.json
	run cat node_modules/is-odd/package.json
	assert_output --partial '"name": "is-odd"'
	assert_output --partial '"version": "3.0.1"'

	# Lockfile records the URL and pinned sha512 integrity.
	run cat aube-lock.yaml
	assert_output --partial "tarball: $url"
	assert_output --partial "integrity: sha512-"
}

@test "aube install resolves transitive deps of a remote tarball" {
	# is-odd@3.0.1 depends on is-number@^6.0.0 — installing from the
	# tarball URL should still pull is-number out of the registry.
	url="${AUBE_TEST_REGISTRY%/}/is-odd/-/is-odd-3.0.1.tgz"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"is-odd":"$url"}}
EOF

	run aube install
	assert_success

	assert_file_exists node_modules/is-odd/package.json
	assert_dir_exists node_modules/.aube
	# is-number is a transitive dep of is-odd@3.0.1; the linker
	# materializes it inside the virtual store but not at the top level.
	run find node_modules/.aube -maxdepth 3 -name is-number
	assert_output --partial is-number
}

@test "lockfile round-trip for remote tarball dep" {
	url="${AUBE_TEST_REGISTRY%/}/is-odd/-/is-odd-3.0.1.tgz"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"is-odd":"$url"}}
EOF

	run aube install
	assert_success
	first_lockfile="$(cat aube-lock.yaml)"

	# Wipe node_modules and reinstall from the lockfile only.
	rm -rf node_modules
	run aube install
	assert_success
	second_lockfile="$(cat aube-lock.yaml)"

	assert_file_exists node_modules/is-odd/package.json
	[ "$first_lockfile" = "$second_lockfile" ]
}
