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
  "license": "Apache-2.0",
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

@test "aube sbom emits CycloneDX licenses for root and dependency components" {
	_setup_mixed_fixture
	run bash -c 'aube sbom | jq -e "
	  .metadata.component.licenses[0].license.id == \"Apache-2.0\"
	  and
	  any(.components[]; .name == \"is-odd\" and .licenses[0].license.id == \"MIT\")
	"'
	assert_success
}

@test "aube sbom finds licenses with a custom virtual store filename limit" {
	cat >package.json <<'JSON'
{
  "name": "sbom-custom-limit",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pre-and-postinstall-scripts-example": "2.0.0"
  }
}
JSON
	run env AUBE_VIRTUAL_STORE_DIR_MAX_LENGTH=40 aube install
	assert_success

	run bash -c 'aube sbom | jq -e "
	  any(.components[]; .name == \"@pnpm.e2e/pre-and-postinstall-scripts-example\" and .licenses[0].license.id == \"MIT\")
	"'
	assert_success
}

@test "aube sbom falls back to install-state license metadata" {
	_setup_mixed_fixture
	run find node_modules/.aube -path '*/node_modules/is-odd/package.json' -delete
	assert_success

	run bash -c 'aube sbom | jq -e "
	  any(.components[]; .name == \"is-odd\" and .licenses[0].license.id == \"MIT\")
	"'
	assert_success
}

@test "aube sbom reads current license metadata from linked packages" {
	mkdir linked
	cat >linked/package.json <<'JSON'
{"name":"linked","version":"1.0.0","license":"MIT"}
JSON
	cat >package.json <<'JSON'
{"name":"sbom-linked","version":"1.0.0","dependencies":{"linked":"link:./linked"}}
JSON
	run aube install
	assert_success
	cat >linked/package.json <<'JSON'
{"name":"linked","version":"1.0.0","license":"Apache-2.0"}
JSON

	run bash -c 'aube sbom | jq -e "
	  any(.components[]; .name == \"linked\" and .licenses[0].license.id == \"Apache-2.0\")
	"'
	assert_success
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
	assert_output --partial 'aube.sh/spdx/'
}

@test "aube sbom --format spdx emits declared but not concluded licenses" {
	_setup_mixed_fixture
	run bash -c 'aube sbom --format spdx | jq -e "
	  any(.packages[]; .SPDXID == \"SPDXRef-Root\" and .licenseDeclared == \"Apache-2.0\" and .licenseConcluded == \"NOASSERTION\")
	  and
	  any(.packages[]; .name == \"is-odd\" and .licenseDeclared == \"MIT\" and .licenseConcluded == \"NOASSERTION\")
	"'
	assert_success
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
	assert_output --partial '"scope": "excluded"'
	assert_output --partial '"cdx:npm:package:development"'
	refute_output --partial 'is-odd@3.0.1'
}

@test "aube sbom marks only dev-only CycloneDX components as excluded" {
	_setup_mixed_fixture
	run bash -c 'aube sbom | jq -e "
	  any(.components[]; .name == \"is-number\" and .scope == \"excluded\" and any(.properties[]?; .name == \"cdx:npm:package:development\" and .value == \"true\"))
	  and
	  any(.components[]; .name == \"is-odd\" and (.scope // \"required\") == \"required\")
	"'
	assert_success
}

@test "aube sbom without a lockfile errors out" {
	cat >package.json <<'JSON'
{ "name": "sbom-nolock", "version": "0.0.1" }
JSON
	run aube sbom
	assert_failure
	assert_output --partial "no lockfile"
}

@test "aube sbom classifies invalid workspace config" {
	_setup_mixed_fixture
	cat >pnpm-workspace.yaml <<'YAML'
packages: [
YAML

	run aube sbom
	assert_failure
	assert_output --partial "ERR_AUBE_WORKSPACE_PARSE"
}

@test "aube sbom filters foreign optional packages unless lockfile-only is requested" {
	if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* || "$(uname -s)" == CYGWIN* ]]; then
		skip "win32 host would install the win32 optional dependency"
	fi
	cat >package.json <<'JSON'
{
  "name": "sbom-platform-filter",
  "version": "1.0.0",
  "optionalDependencies": {
    "aube-test-optional-win32": "1.0.0"
  }
}
JSON
	run aube install
	assert_success
	run grep -F 'aube-test-optional-win32@1.0.0' aube-lock.yaml
	assert_success

	run aube sbom
	assert_success
	refute_output --partial 'aube-test-optional-win32'

	run aube sbom --lockfile-only
	assert_success
	assert_output --partial 'aube-test-optional-win32'
}
