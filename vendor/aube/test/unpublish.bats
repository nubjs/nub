#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_write_unpublishable_pkg() {
	cat >package.json <<-'EOF'
		{
		  "name": "unpublish-smoke",
		  "version": "0.1.0"
		}
	EOF
}

@test "aube unpublish --dry-run uses ./package.json by default" {
	_write_unpublishable_pkg

	run aube unpublish --dry-run --registry=https://r.example.com/
	assert_success
	assert_output --partial "unpublish-smoke@0.1.0"
	assert_output --partial "dry run"
	assert_output --partial "https://r.example.com/"
}

@test "aube unpublish --dry-run with name@version unpublishes a single version" {
	run aube unpublish --dry-run --registry=https://r.example.com/ \
		lodash@4.17.21
	assert_success
	assert_output --partial "lodash@4.17.21"
	assert_output --partial "dry run"
}

@test "aube unpublish bare-name without --force errors" {
	run aube unpublish --dry-run --registry=https://r.example.com/ lodash
	assert_failure
	assert_output --partial "--force"
}

@test "aube unpublish --dry-run --force with bare name reports whole-package intent" {
	run aube unpublish --dry-run --force --registry=https://r.example.com/ lodash
	assert_success
	assert_output --partial "ALL versions"
	assert_output --partial "lodash"
}

@test "aube unpublish --dry-run echoes scoped names in the report" {
	# `--dry-run` only prints the human-readable spec and the registry
	# base URL, not the percent-encoded endpoint path. Encoding is
	# covered by the `encode_package_name` unit tests in commands/mod.rs
	# and exercised live by the `publish` dry-run, which *does* print
	# the URL.
	run aube unpublish --dry-run --registry=https://r.example.com/ \
		'@aube-fixture/demo@1.0.0'
	assert_success
	assert_output --partial "@aube-fixture/demo@1.0.0"
}

@test "aube unpublish errors without an auth token" {
	_write_unpublishable_pkg

	run aube unpublish --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no auth token"
}

@test "aube unpublish missing-auth diagnostics omit registry userinfo and query credentials" {
	_write_unpublishable_pkg

	run aube unpublish --registry='https://unpublish-user:unpublish-password@password-tail@r.example.com/npm?signature=unpublish-signature#unpublish-fragment'
	assert_failure
	assert_output --partial 'aube login --registry https://r.example.com/npm/'
	for secret in unpublish-user unpublish-password password-tail '?signature=' unpublish-signature '#unpublish-fragment' unpublish-fragment; do
		refute_output --partial "$secret"
	done
}

@test "aube unpublish refusal hides registry credentials" {
	_write_unpublishable_pkg
	echo '//127.0.0.1:1/npm/:_authToken=fake' >.npmrc

	run aube unpublish \
		--registry='http://unpublish-user:unpublish-password@password-tail@127.0.0.1:1/npm?signature=unpublish-signature#unpublish-fragment'
	assert_failure
	assert_output --partial "failed to GET"
	assert_output --partial "connection failed"
	for secret in unpublish-user unpublish-password password-tail '?signature=' unpublish-signature '#unpublish-fragment' unpublish-fragment; do
		refute_output --partial "$secret"
	done
}

@test "aube unpublish rescoped registry GET hides credentialed source URL" {
	_write_unpublishable_pkg
	cat >.npmrc <<'EOF'
registry=http://rescope-user:rescope-password@password-tail@127.0.0.1:1/npm?token=opaque@query#rescope-fragment
_authToken=rescoped-token
EOF

	run aube unpublish
	assert_failure
	assert_output --partial "failed to GET"
	assert_output --partial "connection failed"
	for secret in rescope-user rescope-password password-tail '?token=' opaque query '#rescope-fragment' rescope-fragment; do
		refute_output --partial "$secret"
	done
}

@test "aube unpublish errors when ./package.json has no name" {
	cat >package.json <<-'EOF'
		{
		  "version": "0.1.0"
		}
	EOF
	run aube unpublish --dry-run --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no \`name\` field"
}

@test "aube unpublish errors when ./package.json has no version" {
	cat >package.json <<-'EOF'
		{
		  "name": "no-version"
		}
	EOF
	run aube unpublish --dry-run --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no \`version\` field"
}
