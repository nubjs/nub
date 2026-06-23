#!/usr/bin/env bats
#
# Ported from pnpm/test/saveCatalog.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# 6 of 8 tests run against aube's `--save-catalog` / `--save-catalog-name`
# implementation. The two remaining `skip`s are blocked on independent
# feature gaps documented at each test (multi-lockfile workspaces and
# `<pkg>@workspace:*` parsing in `aube add`).
#
# Substitutions for the offline registry (no @pnpm.e2e fixtures yet):
#   @pnpm.e2e/bar    -> is-odd  (versions 0.1.2, 3.0.1)
#   @pnpm.e2e/foo    -> is-even (version 1.0.0)
#   @pnpm.e2e/pkg-a  -> is-odd
#   @pnpm.e2e/pkg-b  -> is-number
#   @pnpm.e2e/pkg-c  -> semver

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube add --save-catalog: writes catalogs to manifest of single-package workspace" {
	# Ported from pnpm/test/saveCatalog.ts:12

	cat >package.json <<'JSON'
{
  "name": "test-save-catalog",
  "version": "0.0.0",
  "private": true,
  "dependencies": { "is-odd": "catalog:" }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: ^3.0.1
YAML

	# Initial install: existing catalog: dep resolves correctly.
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd

	# `aube add --save-catalog` should:
	#   - write `catalog:` (not `^1.0.0`) into package.json dependencies for is-even
	#   - add `is-even: ^1.0.0` into pnpm-workspace.yaml's catalog
	#   - leave the existing is-odd entry untouched
	run aube add --save-catalog is-even@^1.0.0
	assert_success
	run grep -F '"is-even": "catalog:"' package.json
	assert_success
	run grep -F "is-even: ^1.0.0" pnpm-workspace.yaml
	assert_success
	run grep -F "is-odd: ^3.0.1" pnpm-workspace.yaml
	assert_success
}

@test "aube add --save-catalog: writes catalogs in a shared-lockfile workspace" {
	# Ported from pnpm/test/saveCatalog.ts:106
	# Adapted: aube requires a root package.json to mark the workspace
	# project root; pnpm tolerates its absence. The workspace yaml is
	# the workspace marker either way.

	cat >package.json <<'JSON'
{ "name": "save-catalog-shared", "version": "0.0.0", "private": true }
JSON
	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0", "dependencies": { "is-odd": "catalog:" } }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: ^3.0.1
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success
	# Single root lockfile records the catalog.
	assert_file_exists aube-lock.yaml

	# Filtered add into project-1 with --save-catalog: catalog should grow
	# with the is-even entry and project-1's manifest should write `catalog:`.
	run aube --filter=project-1 add --save-catalog is-even@^1.0.0
	assert_success
	run grep -F "is-even: ^1.0.0" pnpm-workspace.yaml
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
	# Existing entry untouched.
	run grep -F "is-odd: ^3.0.1" pnpm-workspace.yaml
	assert_success
}

@test "aube add --save-catalog: writes catalogs in a multi-lockfile workspace" {
	# Ported from pnpm/test/saveCatalog.ts:213
	# Adapted: root package.json added (aube requires it).

	cat >package.json <<'JSON'
{ "name": "save-catalog-multi-lockfile", "version": "0.0.0", "private": true }
JSON
	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0", "dependencies": { "is-odd": "catalog:" } }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
sharedWorkspaceLockfile: false
catalog:
  is-odd: ^3.0.1
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success
	# Each project gets its own lockfile, the workspace root included
	# (pnpm parity: the root is itself a project, so under
	# sharedWorkspaceLockfile=false it writes its own root lockfile).
	assert_file_exists project-0/aube-lock.yaml
	assert_file_exists project-1/aube-lock.yaml
	assert_file_exists aube-lock.yaml

	run aube --filter=project-1 add --save-catalog is-even@^1.0.0
	assert_success
	run grep -F "is-even:" pnpm-workspace.yaml
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog: never adds a workspace: dep to the catalog" {
	# Ported from pnpm/test/saveCatalog.ts:333
	# Adapted: root package.json added (aube requires it).

	cat >package.json <<'JSON'
{ "name": "save-catalog-workspace-spec", "version": "0.0.0", "private": true }
JSON
	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0" }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success

	run aube --filter=project-1 add --save-catalog "project-0@workspace:*"
	assert_success
	# project-0 is a local workspace package — `--save-catalog` must NOT
	# create a catalog entry for it. Easiest invariant: no `catalog:`
	# top-level key was introduced.
	run bash -c "grep -E '^catalog:' pnpm-workspace.yaml || true"
	assert_output ""
	# project-1's manifest writes `workspace:*`, not `catalog:`.
	run grep -F '"project-0": "workspace:*"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog: doesn't catalogize deps that were edited into package.json directly" {
	# Ported from pnpm/test/saveCatalog.ts:392

	cat >package.json <<'JSON'
{
  "name": "test-save-catalog",
  "version": "0.0.0",
  "private": true,
  "dependencies": { "is-odd": "catalog:" }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: 3.0.1
YAML

	run aube install
	assert_success

	# Edit package.json directly to introduce a new bare-spec dep.
	cat >package.json <<'JSON'
{
  "name": "test-save-catalog",
  "version": "0.0.0",
  "private": true,
  "dependencies": {
    "is-odd": "catalog:",
    "is-number": "*"
  }
}
JSON

	# Now add a third dep with --save-catalog. Only the *added* package
	# (semver) should be catalogized; is-number stays as the bare `*` spec.
	run aube add --save-catalog "semver@^7.0.0"
	assert_success
	run grep -F '"is-odd": "catalog:"' package.json
	assert_success
	run grep -F '"is-number": "*"' package.json
	assert_success
	run grep -F '"semver": "catalog:"' package.json
	assert_success
	# Catalog should have is-odd + semver, but NOT is-number.
	run grep -F "is-odd: 3.0.1" pnpm-workspace.yaml
	assert_success
	run grep -F "semver:" pnpm-workspace.yaml
	assert_success
	run grep -F "is-number:" pnpm-workspace.yaml
	assert_failure
}

@test "aube add --save-catalog: never overwrites an existing catalog entry" {
	# Ported from pnpm/test/saveCatalog.ts:488
	# Adapted: root package.json added (aube requires it).

	cat >package.json <<'JSON'
{ "name": "save-catalog-no-overwrite", "version": "0.0.0", "private": true }
JSON
	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0", "dependencies": { "is-odd": "catalog:" } }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	# Catalog deliberately pins an OLD version; --save-catalog should not
	# silently overwrite it, even when adding a higher range for the same pkg.
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: =0.1.2
packages:
  - project-0
  - project-1
YAML

	run aube install
	assert_success

	run aube --filter=project-1 add --save-catalog "is-even@1.0.0" "is-odd@3.0.1"
	assert_success
	# is-odd's existing catalog pin must be preserved.
	run grep -F "is-odd: =0.1.2" pnpm-workspace.yaml
	assert_success
	# is-even is brand new — gets catalogized.
	run grep -F "is-even: 1.0.0" pnpm-workspace.yaml
	assert_success
	# project-1 gets the explicit is-odd@3.0.1 (NOT catalog:, since the
	# existing catalog entry doesn't match), and is-even via catalog:.
	run grep -F '"is-odd": "3.0.1"' project-1/package.json
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog --recursive: seeds the catalog from a no-catalog workspace" {
	# Ported from pnpm/test/saveCatalog.ts:593
	# Adapted: root package.json added (aube requires it).

	cat >package.json <<'JSON'
{ "name": "save-catalog-recursive", "version": "0.0.0", "private": true }
JSON
	mkdir -p project-0 project-1
	cat >project-0/package.json <<'JSON'
{ "name": "project-0", "version": "0.0.0" }
JSON
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "0.0.0" }
JSON
	# Note: pnpm-workspace.yaml has packages but no catalog. Recursive
	# --save-catalog should seed it.
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-0
  - project-1
YAML

	run aube --recursive add --save-catalog "is-even@1.0.0"
	assert_success
	run grep -F "is-even: 1.0.0" pnpm-workspace.yaml
	assert_success
	# Both projects now reference the catalog.
	run grep -F '"is-even": "catalog:"' project-0/package.json
	assert_success
	run grep -F '"is-even": "catalog:"' project-1/package.json
	assert_success
}

@test "aube add --save-catalog-name=<name>: writes into a named catalog" {
	# Ported from pnpm/test/saveCatalog.ts:672

	cat >package.json <<'JSON'
{
  "name": "test-save-catalog-name",
  "version": "0.0.0",
  "private": true,
  "dependencies": { "is-odd": "catalog:" }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
catalog:
  is-odd: ^3.0.1
YAML

	run aube install
	assert_success

	# Add into a named catalog rather than `default`. Manifest specifier
	# should be `catalog:my-catalog`, and the named catalog should appear
	# under `catalogs:` (plural) in the workspace yaml.
	run aube add --save-catalog-name=my-catalog "is-even@^1.0.0"
	assert_success
	run grep -F '"is-even": "catalog:my-catalog"' package.json
	assert_success
	run grep -F "my-catalog:" pnpm-workspace.yaml
	assert_success
	run grep -F "is-even: ^1.0.0" pnpm-workspace.yaml
	assert_success
	# is-odd's default catalog entry stays put.
	run grep -F "is-odd: ^3.0.1" pnpm-workspace.yaml
	assert_success
}

@test "aube add --save-catalog conflicts with --no-save" {
	# `--no-save` snapshots and restores package.json + the lockfile,
	# but the workspace yaml is outside that snapshot. Combining the
	# two would orphan the catalog entry. clap should reject the combo
	# up front rather than letting the install run silently corrupt
	# pnpm-workspace.yaml.
	cat >package.json <<'JSON'
{ "name": "save-catalog-no-save-conflict", "version": "0.0.0" }
JSON

	run aube add --save-catalog --no-save is-odd
	assert_failure
	assert_output --partial "cannot be used with"

	run aube add --save-catalog-name=foo --no-save is-odd
	assert_failure
	assert_output --partial "cannot be used with"
}
