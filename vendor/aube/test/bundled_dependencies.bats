#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
	# Our Verdaccio fixture strips `bundledDependencies` when it
	# projects stored packuments to corgi (`application/vnd.npm.install-v1+json`),
	# so the resolver would never see the field under normal operation
	# against this registry. The internal env var below forces the
	# unabbreviated packument purely for this fixture. Production
	# registries (npmjs.org) return `bundleDependencies` in corgi per
	# the npm spec, so no production user ever needs this override —
	# which also means the *corgi-decoded* bundled-deps path is
	# exercised only by the `aube-manifest` unit tests that deserialize
	# raw JSON, not by an end-to-end install. If someone touches the
	# resolver's packument parsing and wants a second e2e signal,
	# swapping Verdaccio for a registry that honors
	# `bundleDependencies` in corgi (or patching Verdaccio's
	# abbreviate filter) would let this test run without the env var.
	export AUBE_INTERNAL_FORCE_FULL_PACKUMENT=1
}

teardown() {
	_common_teardown
}

# The `aube-bundled-host` fixture ships with a `node_modules/aube-bundled-child`
# tree baked into its tarball. The registry has no packument for
# `aube-bundled-child`, so if the resolver were to recurse into it the
# install would fail. A passing install therefore proves the resolver
# honored `bundledDependencies` and skipped the registry lookup entirely.

@test "aube install serves bundledDependencies from the parent tarball" {
	cat >package.json <<'JSON'
{
  "name": "bundled-deps-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-bundled-host": "1.0.0"
  }
}
JSON
	run aube install
	assert_success

	# Bundled child lives inside the parent's own node_modules, not as a
	# sibling entry in .aube.
	assert_file_exists \
		node_modules/.aube/aube-bundled-host@1.0.0/node_modules/aube-bundled-host/node_modules/aube-bundled-child/index.js

	# Nothing under .aube should reference aube-bundled-child at the top
	# level — that would mean the resolver fetched it (impossible here,
	# since the fixture registry has no such packument).
	run bash -c "ls node_modules/.aube | grep '^aube-bundled-child' || true"
	assert_output ""

	# The bundled child's bin is hoisted into the project .bin dir by the
	# linker's post-bin-linking pass.
	assert_file_exists node_modules/.bin/aube-bundled-cli

	# And `require('aube-bundled-host')` transitively loads the bundled
	# child via Node's directory walk.
	run node -e "console.log(require('aube-bundled-host'))"
	assert_success
	assert_output --partial "hello from bundled child"
}

@test "aube install hoists bundled bins into workspace package .bin dirs" {
	# Two workspace packages; only `packages/app` depends on the
	# bundled-host package. Its own `node_modules/.bin` must contain
	# the bundled child's `aube-bundled-cli` even though the root
	# importer doesn't declare the dep.
	mkdir -p packages/app packages/other
	cat >package.json <<'JSON'
{
  "name": "bundled-deps-ws-root",
  "version": "1.0.0",
  "private": true
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	cat >packages/app/package.json <<'JSON'
{
  "name": "@ws/app",
  "version": "1.0.0",
  "dependencies": {
    "aube-bundled-host": "1.0.0"
  }
}
JSON
	cat >packages/other/package.json <<'JSON'
{
  "name": "@ws/other",
  "version": "1.0.0"
}
JSON
	run aube install
	assert_success

	assert_file_exists packages/app/node_modules/.bin/aube-bundled-cli
	assert_file_not_exists packages/other/node_modules/.bin/aube-bundled-cli
}

@test "aube install records bundledDependencies on the lockfile snapshot" {
	cat >package.json <<'JSON'
{
  "name": "bundled-deps-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-bundled-host": "1.0.0"
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml

	run grep -F -- "bundledDependencies:" aube-lock.yaml
	assert_success
	run grep -F -- "- aube-bundled-child" aube-lock.yaml
	assert_success
}
