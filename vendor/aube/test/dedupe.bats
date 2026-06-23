#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube dedupe --help" {
	run aube dedupe --help
	assert_success
	assert_output --partial "Re-resolve the lockfile"
	assert_output --partial "--check"
}

@test "aube dedupe on a fresh lockfile reports already deduped" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube dedupe
	assert_success
	assert_output --partial "Lockfile is already deduped"
	assert_output --partial "7 packages"
}

@test "aube dedupe --check exits 0 when lockfile is already deduped" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube dedupe --check
	assert_success
	assert_output --partial "already deduped"
}

@test "aube dedupe is idempotent" {
	_setup_basic_fixture
	run aube install
	assert_success

	# Capture lockfile bytes, run dedupe, verify no byte-level change.
	cp aube-lock.yaml aube-lock.yaml.before

	run aube dedupe
	assert_success

	run cmp -s aube-lock.yaml aube-lock.yaml.before
	assert_success
}

@test "aube dedupe leaves a working node_modules on a clean install" {
	_setup_basic_fixture
	run aube install
	assert_success

	run aube dedupe
	assert_success

	assert_dir_exists node_modules/is-odd
	assert_dir_exists node_modules/is-even
}

@test "aube dedupe errors cleanly without package.json" {
	run aube dedupe
	assert_failure
	assert_output --partial "package.json"
}

@test "aube dedupe removes orphan entries from the lockfile" {
	# Craft a manifest with a single dep plus a committed lockfile that
	# has an extra orphan package — something not referenced by any
	# importer. Fresh re-resolution produces a graph without the orphan,
	# so dedupe reports it as removed and strips it from the lockfile.
	cat >package.json <<'EOF'
{
  "name": "aube-test-dedupe",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" }
}
EOF
	cat >aube-lock.yaml <<'EOF'
lockfileVersion: '9.0'
settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false
importers:
  .:
    dependencies:
      is-odd:
        specifier: ^3.0.1
        version: 3.0.1
packages:
  is-odd@3.0.1:
    resolution:
      integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==
  is-number@6.0.0:
    resolution:
      integrity: sha512-Wu1VHeILBK8KAWJUAiSZQX94GmOE45Rg6/538fKwiloUu21KncEkYGPqob2oSZ5mUT73vLGrHQjKw3KMPwfDzg==
  orphan-pkg@1.0.0:
    resolution:
      integrity: sha512-0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000==
snapshots:
  is-odd@3.0.1:
    dependencies:
      is-number: 6.0.0
  is-number@6.0.0: {}
  orphan-pkg@1.0.0: {}
EOF

	# --check should detect the orphan and exit non-zero
	run aube dedupe --check
	assert_failure
	assert_output --partial "- orphan-pkg@1.0.0"
	assert_output --partial "lockfile is not deduped"

	# Actual dedupe should remove the orphan and succeed
	run aube dedupe
	assert_success
	assert_output --partial "- orphan-pkg@1.0.0"

	# The orphan must be gone from the written lockfile
	run grep -F "orphan-pkg" aube-lock.yaml
	assert_failure

	# Real deps should still be present
	run grep -F "is-odd@3.0.1" aube-lock.yaml
	assert_success

	# Second dedupe is a no-op
	run aube dedupe
	assert_success
	assert_output --partial "already deduped"
}
