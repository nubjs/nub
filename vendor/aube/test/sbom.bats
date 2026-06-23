#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_mixed_fixture() {
	cat >package.json <<'JSON'
{
  "name": "sbom-test",
  "version": "1.2.3",
  "dependencies": {
    "is-odd": "^3.0.1"
  },
  "devDependencies": {
    "is-number": "^7.0.0"
  }
}
JSON
	run aube install
	assert_success
}

@test "aube sbom emits CycloneDX 1.5 JSON by default" {
	_setup_mixed_fixture
	run aube sbom
	assert_success
	assert_output --partial '"bomFormat": "CycloneDX"'
	assert_output --partial '"specVersion": "1.5"'
	assert_output --partial '"pkg:npm/is-odd@3.0.1"'
	assert_output --partial '"pkg:npm/is-number@7.0.0"'
}

@test "aube sbom --format spdx emits SPDX 2.3 JSON" {
	_setup_mixed_fixture
	run aube sbom --format spdx
	assert_success
	assert_output --partial '"spdxVersion": "SPDX-2.3"'
	assert_output --partial '"SPDXRef-DOCUMENT"'
	assert_output --partial '"pkg:npm/is-odd@3.0.1"'
	assert_output --partial 'DEPENDS_ON'
	# SPDXRef-Root must have outgoing DEPENDS_ON edges to its direct deps,
	# not just inter-package edges between closure entries.
	assert_output --partial '"spdxElementId": "SPDXRef-Root"'
	assert_output --partial 'aube.jdx.dev/spdx/'
}

@test "aube sbom --prod drops devDependencies" {
	_setup_mixed_fixture
	run aube sbom --prod
	assert_success
	assert_output --partial '"pkg:npm/is-odd@3.0.1"'
	refute_output --partial 'is-number@7.0.0'
}

@test "aube sbom --dev keeps only devDependencies" {
	_setup_mixed_fixture
	run aube sbom --dev
	assert_success
	assert_output --partial '"pkg:npm/is-number@7.0.0"'
	refute_output --partial 'is-odd@3.0.1'
}

@test "aube sbom without a lockfile errors out" {
	cat >package.json <<'JSON'
{ "name": "sbom-nolock", "version": "0.0.1" }
JSON
	run aube sbom
	assert_failure
	assert_output --partial "no lockfile"
}
