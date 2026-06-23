#!/usr/bin/env bats
#
# Tests for `aube approve-builds` and `aube ignored-builds`. Uses the
# same `aube-test-builds-marker` fixture package as `allow_builds.bats`:
# it declares a `postinstall` that writes a marker file, so its
# presence after a second install proves the approve-builds write
# round-tripped into the project's `allowBuilds`.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "install warns about ignored build scripts" {
	cat >package.json <<'JSON'
{
  "name": "install-ignored-warn-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "ignored build scripts"
	assert_output --partial "aube-test-builds-marker"
	assert_output --partial "aube approve-builds"
}

@test "install does not warn when build is allowed" {
	cat >package.json <<'JSON'
{
  "name": "install-no-warn-allowed-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	run aube install
	assert_success
	refute_output --partial "ignored build scripts"
}

@test "install does not warn when --ignore-scripts is set" {
	cat >package.json <<'JSON'
{
  "name": "install-no-warn-ignore-scripts-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install --ignore-scripts
	assert_success
	refute_output --partial "ignored build scripts"
}

@test "ignored-builds lists deps whose scripts were skipped" {
	cat >package.json <<'JSON'
{
  "name": "ignored-builds-list-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success

	run aube ignored-builds
	assert_success
	assert_output --partial "aube-test-builds-marker"
}

@test "ignored-builds reports nothing when everything is allowed" {
	cat >package.json <<'JSON'
{
  "name": "ignored-builds-empty-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	run aube install
	assert_success

	run aube ignored-builds
	assert_success
	assert_output --partial "No ignored builds"
}

@test "install leaves package.json untouched when builds are unreviewed" {
	# Diverges from pnpm: pnpm rewrites `package.json#pnpm.onlyBuiltDependencies`
	# (or the placeholder map) on every install when it discovers an unreviewed
	# build. Aube keeps the manifest pristine — the warning + `aube approve-builds`
	# flow is enough.
	cat >package.json <<'JSON'
{
  "name": "approve-builds-all-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	cp package.json package.json.before
	run aube install
	assert_success
	assert_file_not_exists aube-builds-marker.txt
	assert_file_not_exists aube-workspace.yaml
	run diff -u package.json.before package.json
	assert_success

	run aube approve-builds --all
	assert_success
	assert_output --partial "aube-test-builds-marker"
	assert_output --partial "package.json"

	run grep -q '"aube-test-builds-marker": true' package.json
	assert_success
	run grep -q 'onlyBuiltDependencies' package.json
	assert_failure

	# A re-install should run the previously-ignored postinstall.
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "approve-builds --all in npm-style monorepo writes package.json allowBuilds" {
	# An npm/yarn-style monorepo carries `workspaces` directly in
	# package.json. With no workspace yaml and no `pnpm` namespace,
	# the unified writer rule lands the review map in
	# package.json#aube.allowBuilds rather than spawning a fresh
	# aube-workspace.yaml.
	mkdir -p packages/app
	cat >package.json <<'JSON'
{
  "name": "monorepo-root",
  "version": "1.0.0",
  "private": true,
  "workspaces": ["packages/*"]
}
JSON
	cat >packages/app/package.json <<'JSON'
{
  "name": "app",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success

	run aube approve-builds --all
	assert_success
	assert_output --partial "aube-test-builds-marker"

	assert_file_not_exists aube-workspace.yaml
	assert_file_not_exists pnpm-workspace.yaml

	run grep -q '"allowBuilds"' package.json
	assert_success
	run grep -q '"aube-test-builds-marker": true' package.json
	assert_success

	# Round-trip: a re-install must honor the policy and run the
	# previously-skipped postinstall.
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "approve-builds merges into an existing pnpm-workspace.yaml" {
	cat >package.json <<'JSON'
{
  "name": "approve-builds-merge-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - 'packages/*'
allowBuilds:
  some-other-pkg: true
YAML
	run aube install
	assert_success

	run aube approve-builds --all
	assert_success

	# Existing keys stay, the new entry is appended, and the sibling
	# entry isn't duplicated.
	run grep -q '^packages:' pnpm-workspace.yaml
	assert_success
	run grep -q '  some-other-pkg: true' pnpm-workspace.yaml
	assert_success
	run grep -q '  aube-test-builds-marker: true' pnpm-workspace.yaml
	assert_success
}

@test "approve-builds --all is a no-op when nothing is ignored" {
	cat >package.json <<'JSON'
{
  "name": "approve-builds-noop-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	run aube install
	assert_success

	run aube approve-builds --all
	assert_success
	assert_output --partial "No ignored builds"
}

@test "approve-builds without a TTY requires --all" {
	cat >package.json <<'JSON'
{
  "name": "approve-builds-tty-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success

	run aube approve-builds
	assert_failure
	assert_output --partial "--all"
}
