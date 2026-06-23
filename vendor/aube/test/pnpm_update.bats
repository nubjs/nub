#!/usr/bin/env bats
#
# Ported from pnpm/test/update.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# These tests mutate `dist-tags` on the committed Verdaccio storage via
# `add_dist_tag` and restore them via `git checkout` in teardown — same
# pattern as test/deprecate.bats. Tag the file as serial and disable
# within-file parallelism.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	# Restore any mutated dist-tags so the fixture stays clean across runs.
	if [ -n "${PROJECT_ROOT:-}" ]; then
		git -C "$PROJECT_ROOT" checkout -- \
			test/registry/storage/@pnpm.e2e/foo/package.json \
			test/registry/storage/@pnpm.e2e/bar/package.json \
			test/registry/storage/@pnpm.e2e/dep-of-pkg-with-1-dep/package.json \
			test/registry/storage/@pnpm.e2e/has-prerelease/package.json \
			test/registry/storage/@pnpm.e2e/pkg-with-1-dep/package.json \
			test/registry/storage/@pnpm.e2e/qar/package.json 2>/dev/null || true
	fi
	_common_teardown
}

# Skip if the local Verdaccio fixture isn't running. add_dist_tag mutates
# its on-disk storage, so without it these tests have nothing to PUT.
_require_registry() {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

@test "aube update --latest <pkg>: bumps a single dep past its declared range" {
	# Ported from pnpm/test/update.ts:14 ('update <dep>').
	# pnpm: `pnpm update <pkg>@latest`. aube does not parse `<pkg>@<spec>`
	# in update args, so translate to `aube update --latest <pkg>` — same
	# behavior: rewrite the manifest range to track the resolved version
	# rather than the existing range.
	_require_registry

	# Pin 100.0.0 as latest while the user installs at the lower range,
	# then publish 101.0.0 as the new latest before running update.
	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-dep",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0'
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0

	run aube update --latest '@pnpm.e2e/dep-of-pkg-with-1-dep'
	assert_success

	# Lockfile resolves to the new latest.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success

	# package.json range bumped to track the new version.
	run grep '"\^101.0.0"' package.json
	assert_success
}

@test "aube update --no-save: refreshes the lockfile, leaves package.json range alone" {
	# Ported from pnpm/test/update.ts:34 ('update --no-save').
	# `--no-save` without `--latest` is a no-op for the manifest in aube
	# (plain `update` already doesn't rewrite specifiers), so the assertion
	# shape matches pnpm: lockfile resolves to the new latest in-range,
	# package.json keeps the original `^100.0.0`.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-no-save",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON

	run aube update --no-save
	assert_success

	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success

	# package.json range untouched.
	run grep '"\^100.0.0"' package.json
	assert_success
}

@test "aube update --depth: parsed-but-warn (pnpm parity, no-op)" {
	# Triaged won't-support in test/PNPM_TEST_IMPORT.md (update.ts:599):
	# pnpm's `--depth N` controls how deep the update walks. aube only
	# refreshes direct deps, so the flag is a no-op — warn once with
	# the `rm aube-lock.yaml && aube install` workaround for the real
	# `--depth Infinity` use case.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-depth-warn",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON

	run aube update --depth Infinity
	assert_success
	assert_output --partial '--depth Infinity is ignored'
	assert_output --partial 'rm aube-lock.yaml && aube install'

	# Bare update (no --depth) does not emit the warning.
	run aube update
	assert_success
	refute_output --partial '--depth'
}

@test "aube update -r --depth: warns once across workspace fanout" {
	# Regression for the recursion footgun: `run_filtered` clones the
	# args for each matched workspace package and re-invokes `run`, so
	# without clearing `args.depth` on the per-pkg clone the warning
	# fires 1 + N times instead of once.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0

	mkdir -p p1 p2 p3
	cat >p1/package.json <<'JSON'
{ "name": "p1", "version": "0.0.0", "dependencies": { "@pnpm.e2e/foo": "^100.0.0" } }
JSON
	cat >p2/package.json <<'JSON'
{ "name": "p2", "version": "0.0.0", "dependencies": { "@pnpm.e2e/bar": "^100.0.0" } }
JSON
	cat >p3/package.json <<'JSON'
{ "name": "p3", "version": "0.0.0", "dependencies": { "@pnpm.e2e/foo": "^100.0.0" } }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - p1
  - p2
  - p3
YAML

	run aube update -r --depth Infinity
	assert_success

	# Exactly one occurrence of the warning across the three projects.
	count=$(printf '%s\n' "$output" | grep -c '\-\-depth Infinity is ignored' || true)
	[ "$count" = "1" ]
}

@test "aube update --latest --prod: bumps prod deps, leaves devDeps pinned" {
	# Ported from pnpm/test/update.ts:225 ('update --latest --prod').
	# aube's `add` defaults to prod (no `-P` flag — pnpm requires it for
	# parity with npm), so the second `add` here drops `-P`.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-prod",
  "version": "0.0.0"
}
JSON

	run aube add -D '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0'
	assert_success
	run aube add '@pnpm.e2e/bar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0

	run aube update --latest --prod
	assert_success

	# Prod dep bumped past its range.
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success

	# Dev dep stays pinned at 100.0.0 — --prod skipped it.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_failure
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"' package.json
	assert_success
}

@test "aube update -r --no-save: refreshes a workspace lockfile, leaves manifests alone" {
	# Ported from pnpm/test/update.ts:72 ('recursive update --no-save').
	# sharedWorkspaceLockfile=true writes the refreshed importer back
	# into the workspace root lockfile.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "project",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project
YAML

	run aube update -r --no-save
	assert_success

	# Workspace lockfile carries the bumped version; the filtered
	# project does not keep its own lockfile.
	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success
	assert_file_not_exists project/aube-lock.yaml

	# Project manifest range unchanged.
	run grep '"\^100.0.0"' project/package.json
	assert_success
}

@test "aube up from a workspace member writes the shared root lockfile" {
	cat >package.json <<'JSON'
{ "workspaces": ["sub", "lib"] }
JSON
	mkdir sub lib
	cat >sub/package.json <<'JSON'
{ "name": "sub", "version": "0.0.0", "dependencies": { "lib": "workspace:*" } }
JSON
	cat >lib/package.json <<'JSON'
{ "name": "lib", "version": "1.0.0" }
JSON

	cd sub
	run aube install
	assert_success
	assert_file_exists ../aube-lock.yaml
	assert_file_not_exists aube-lock.yaml

	run aube up
	assert_success
	assert_file_exists ../aube-lock.yaml
	assert_file_not_exists aube-lock.yaml
	run grep 'sub:' ../aube-lock.yaml
	assert_success
}

@test "aube update -r --no-shared-workspace-lockfile: writes a per-project lockfile" {
	# Ported from pnpm/test/update.ts:118 ('recursive update
	# --no-shared-workspace-lockfile').
	# pnpm exposes this as a CLI flag; aube reads
	# `sharedWorkspaceLockfile` from `.npmrc` / pnpm-workspace.yaml /
	# env vars (no CLI flag yet). Set it via `.npmrc` to opt out, then
	# verify the per-project lockfile lands in `project/aube-lock.yaml`
	# instead of the workspace root.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "project",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project
YAML
	cat >>.npmrc <<'EOF'
shared-workspace-lockfile=false
EOF

	run aube update -r --latest
	assert_success

	# Per-project lockfile carries the bumped version.
	assert_file_exists project/aube-lock.yaml
	run grep '@pnpm.e2e/foo@100.1.0' project/aube-lock.yaml
	assert_success

	# Manifest rewritten to track the new latest.
	run grep '"@pnpm.e2e/foo": "\^100.1.0"' project/package.json
	assert_success

	# No root lockfile: this workspace root is config-only (a
	# pnpm-workspace.yaml with no package.json), so it isn't a project
	# and pnpm writes no lockfile for it — only the member does.
	assert_file_not_exists aube-lock.yaml
}

@test "aube update -r --latest: bumps every workspace project's manifest" {
	# Ported from pnpm/test/update.ts:426 ('recursive update --latest on
	# projects with a shared a lockfile'). The `@pnpm.e2e/qar` alias dep
	# is omitted (no fixture mirrored yet).
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0",
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/bar": "100.0.0",
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	run aube update -r --latest
	assert_success

	# Both manifests rewritten to the new latest.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "101.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.1.0"' project-2/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-2/package.json
	assert_success

	# The shared workspace lockfile carries both bumped importers.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	assert_file_not_exists project-1/aube-lock.yaml
	assert_file_not_exists project-2/aube-lock.yaml
}

@test "aube update -F writes the workspace root lockfile" {
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/bar": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	run aube install
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	run aube update -F project-1 --latest '@pnpm.e2e/foo'
	assert_success

	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success
	run grep 'project-2:' aube-lock.yaml
	assert_success
	assert_file_not_exists project-1/aube-lock.yaml
	assert_file_not_exists project-2/aube-lock.yaml
}

@test "aube update -F keeps the root lockfile when the workspace root is matched" {
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0

	mkdir project
	cat >package.json <<'JSON'
{
  "name": "workspace-root",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >project/package.json <<'JSON'
{
  "name": "project",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/bar": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project
includeWorkspaceRoot: true
YAML

	run aube install
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	run aube update -F workspace-root --latest '@pnpm.e2e/foo'
	assert_success

	assert_file_exists aube-lock.yaml
	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success
	run grep 'project:' aube-lock.yaml
	assert_success
	assert_file_not_exists project/aube-lock.yaml
}

@test "aube update -r --latest --prod: skips devDeps in workspace fanout" {
	# Ported from pnpm/test/update.ts:478 ('recursive update --latest
	# --prod on projects with a shared a lockfile'). Verifies the
	# prod/dev split survives the recursive fanout.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"
  },
  "devDependencies": {
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "100.0.0"
  },
  "devDependencies": {
    "@pnpm.e2e/bar": "100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	run aube update -r --latest --prod
	assert_success

	# Prod deps bumped past their pins.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "101.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-2/package.json
	assert_success

	# Dev deps left alone — --prod skipped them.
	run grep '"@pnpm.e2e/foo": "100.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.0.0"' project-2/package.json
	assert_success
}

@test "aube update --latest <pkg>: downgrades prerelease to the latest dist-tag" {
	# Ported from pnpm/test/update.ts:659 ('update with tag @latest will
	# downgrade prerelease'). pnpm uses `pnpm update <pkg>@latest` to
	# force the latest dist-tag; aube doesn't parse `<pkg>@<spec>` in
	# update args (see PNPM_TEST_IMPORT.md), so translate to
	# `aube update --latest <pkg>` — same effect: rewrite the manifest
	# to track the resolved version, even when that downgrades a
	# prerelease pin.
	_require_registry

	add_dist_tag '@pnpm.e2e/has-prerelease' latest 2.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-prerelease-downgrade",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/has-prerelease@3.0.0-rc.0'
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_success

	run aube update --latest '@pnpm.e2e/has-prerelease'
	assert_success

	# Manifest now points at the dist-tag's resolved version.
	run grep '"@pnpm.e2e/has-prerelease": "2.0.0"' package.json
	assert_success
	run grep '@pnpm.e2e/has-prerelease@2.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_failure
}

@test "aube update --latest: bumps prod deps, npm: aliases, and ranges" {
	# Ported from pnpm/test/update.ts:143 ('update --latest').
	# Drops the `kevva/is-negative` GitHub-shorthand dep — aube has no
	# resolver for `user/repo` shorthands. Without the GitHub dep, the
	# remaining shape (range pin + npm: alias + caret range) is the
	# regression guard for `update --latest` rewriting every direct dep.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0' '@pnpm.e2e/bar@^100.0.0' 'alias@npm:@pnpm.e2e/qar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update --latest
	assert_success

	# All three direct deps bumped past their original ranges in the lockfile.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep 'alias@100.1.0' aube-lock.yaml
	assert_success

	# Manifest specifiers tracked the new versions, preserving caret +
	# `npm:` alias prefix.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "\^101.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@\^100.1.0"' package.json
	assert_success
}

@test "aube update --latest -E: rewrites manifest specs as exact pins" {
	# Ported from pnpm/test/update.ts:170 ('update --latest --save-exact').
	# pnpm's `--save-exact` (alias `-E`) drops the caret on the rewritten
	# specifier. GitHub-shorthand dep dropped (see misc.ts:143 port).
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-exact",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' '@pnpm.e2e/bar@100.0.0' 'alias@npm:@pnpm.e2e/qar@100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update --latest -E
	assert_success

	# Lockfile carries the new versions.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep 'alias@100.1.0' aube-lock.yaml
	assert_success

	# Manifest specs are exact pins (no caret), npm: alias preserved.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "101.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@100.1.0"' package.json
	assert_success
}

@test "aube update --latest <name>: bumps named deps, leaves others pinned" {
	# Ported from pnpm/test/update.ts:197 ('update --latest specific
	# dependency'). pnpm uses `pnpm update -L @pnpm.e2e/bar alias
	# is-negative`; the `is-negative` GitHub dep is dropped (see
	# misc.ts:143 port). aube's `-L` is the same flag (--latest short).
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-specific",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' '@pnpm.e2e/bar@^100.0.0' '@pnpm.e2e/foo@100.0.0' 'alias@npm:@pnpm.e2e/qar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update -L '@pnpm.e2e/bar' alias
	assert_success

	# Named deps bumped: bar (range, caret preserved) and alias (npm: alias).
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success
	run grep 'alias@100.1.0' aube-lock.yaml
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@\^100.1.0"' package.json
	assert_success

	# Unnamed deps stay at their original pins — both lockfile and manifest.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"' package.json
	assert_success
	run grep '@pnpm.e2e/foo@100.0.0' aube-lock.yaml
	assert_success
	run grep '"@pnpm.e2e/foo": "100.0.0"' package.json
	assert_success
}

@test "aube update -r --latest <name>: bumps named deps across workspace" {
	# Ported from pnpm/test/update.ts:369 ('recursive update --latest
	# specific dependency on projects that do not share a lockfile').
	# Verifies the workspace fanout honors named-dep filtering: only
	# `@pnpm.e2e/foo` and `alias` (the npm: alias) are bumped; everything
	# else stays at its original pin.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "alias": "npm:@pnpm.e2e/qar@100.0.0",
    "@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0",
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/bar": "100.0.0",
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update -r --latest '@pnpm.e2e/foo' alias
	assert_success

	# project-1: foo + alias bumped; the rest left alone.
	run grep '"@pnpm.e2e/foo": "\^100.1.0"' project-1/package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@100.1.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"' project-1/package.json
	assert_success

	# project-2: foo bumped; bar untouched (not in the named-deps list).
	run grep '"@pnpm.e2e/foo": "\^100.1.0"' project-2/package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.0.0"' project-2/package.json
	assert_success
}

@test "aube update -r --latest <name>: writes shared workspace lockfile" {
	# Ported from pnpm/test/update.ts:543 ('recursive update --latest
	# specific dependency on projects with a shared a lockfile'). The
	# package layout here uses exact pins instead of caret ranges
	# (matching the pnpm fixture at :551-571).
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "alias": "npm:@pnpm.e2e/qar@100.0.0",
    "@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0",
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/bar": "100.0.0",
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update -r --latest '@pnpm.e2e/foo' alias
	assert_success

	run grep '"@pnpm.e2e/foo": "100.1.0"' project-1/package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@100.1.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-2/package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.0.0"' project-2/package.json
	assert_success
}

@test "aube update -r --prod <name>: skips projects where the dep is only a devDep" {
	# Regression guard for the per-project arg filter: when `--prod` is
	# active, a named arg that exists only as a devDep in some project
	# must NOT count as "declared" in that project — otherwise the
	# fanout passes the arg into `run` and `run` hard-errors with
	# 'package X is not a dependency' (its inner all_specifiers
	# excludes devDeps under --prod). The bucket filter in
	# `run_filtered` mirrors `run`'s include_prod/include_dev/
	# include_optional logic so each project's "declared" set matches
	# the one `run` will look up.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0

	mkdir project-1 project-2
	# project-1: foo as a prod dep — should be bumped.
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	# project-2: foo as a devDep only — should be SKIPPED (not errored)
	# under --prod.
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/bar": "100.0.0"
  },
  "devDependencies": {
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	# Without the bucket filter this hard-errors on project-2.
	run aube update -r --latest --prod '@pnpm.e2e/foo'
	assert_success

	# project-1's prod foo got bumped.
	run grep '"@pnpm.e2e/foo": "100.1.0"' project-1/package.json
	assert_success

	# project-2's devDep foo left at 100.0.0 — --prod skipped the project.
	run grep '"@pnpm.e2e/foo": "100.0.0"' project-2/package.json
	assert_success
}

@test "aube update --latest: keeps a higher-than-latest prerelease pin" {
	# Ported from pnpm/test/update.ts:615 ('update to latest without
	# downgrading already defined prerelease (#7436)'). With latest=2.0.0
	# and the user pinned at 3.0.0-rc.0, `update --latest` (bulk, no
	# package args) must NOT downgrade the manifest to 2.0.0 — the
	# prerelease wins because it's numerically newer.
	#
	# pnpm also runs `update -r --latest` against the same single-project
	# fixture. aube's `-r` requires a workspace root, so the recursive
	# half is covered by the workspace ports below (728, 807) rather
	# than re-running it here.
	_require_registry

	add_dist_tag '@pnpm.e2e/has-prerelease' latest 2.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-prerelease-keep",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/has-prerelease@3.0.0-rc.0'
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/has-prerelease@2.0.0' aube-lock.yaml
	assert_failure

	run aube update --latest
	assert_success

	# Manifest still pinned at the prerelease, lockfile too.
	run grep '"@pnpm.e2e/has-prerelease": "3.0.0-rc.0"' package.json
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/has-prerelease@2.0.0' aube-lock.yaml
	assert_failure
}

@test "aube update --latest: reports correct current version when same name appears transitively" {
	# Discussion #623: when a direct dep shares its name with a
	# transitive snapshot at a different version (here `dep-of-pkg-with-1-dep@101.0.0`
	# direct via catalog vs `@100.0.0` pulled in transitively by abc),
	# the post-resolve report's "current" must come from the importer's
	# resolved edge — not whichever dep_path comes first in BTreeMap
	# iteration. The pre-fix code printed `100.0.0 (already latest)`
	# (the wrong, transitive version) for catalog deps that were already
	# at the registry's latest.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0

	cat >package.json <<'JSON'
{
  "name": "aube-update-dup-name",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/dep-of-pkg-with-1-dep": "catalog:",
    "@pnpm.e2e/abc": "2.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "."
catalog:
  "@pnpm.e2e/dep-of-pkg-with-1-dep": 101.0.0
YAML

	run aube install
	assert_success

	# Sanity: lockfile carries both versions — direct catalog at 101.0.0,
	# transitive (pinned by abc@2.0.0) at 100.0.0.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	run aube update --latest
	assert_success

	# Report must reflect the *direct* version (101.0.0), not the
	# transitive 100.0.0 the buggy lookup picked first by lex order.
	assert_output --partial '101.0.0 (already latest)'
	refute_output --partial '100.0.0 (already latest)'

	# Catalog spec is preserved, both lockfile snapshots intact.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "catalog:"' package.json
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success
}

@test "aube update --latest: keeps a higher-than-latest caret-prerelease range" {
	# Discussion #623: same #7436 guard, extended to caret/tilde
	# prereleases. The original guard only covered exact pins, so
	# `"^3.0.0-rc.0"` (or any non-exact spec resolving above the
	# dist-tag latest) silently downgraded to the registry's latest.
	# The fix uses the LOCKED version, not just the manifest spec, so
	# anything that resolved above latest is preserved regardless of
	# spec shape.
	_require_registry

	add_dist_tag '@pnpm.e2e/has-prerelease' latest 2.0.0

	# Hand-roll the manifest with a caret-prefixed prerelease — `aube
	# add @pkg@3.0.0-rc.0` would save it as an exact pin, masking the
	# bug. The manifest spec "^3.0.0-rc.0" resolves to 3.0.0-rc.0 (the
	# only version satisfying the range that includes a prerelease) but
	# wouldn't get caught by the old `exact_pin_version` check.
	cat >package.json <<'JSON'
{
  "name": "aube-update-caret-prerelease",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/has-prerelease": "^3.0.0-rc.0"
  }
}
JSON

	run aube install
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_success

	run aube update --latest
	assert_success

	# Manifest spec preserved verbatim: no downgrade to "^2.0.0".
	run grep '"@pnpm.e2e/has-prerelease": "\^3.0.0-rc.0"' package.json
	assert_success
	run grep '@pnpm.e2e/has-prerelease@3.0.0-rc.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/has-prerelease@2.0.0' aube-lock.yaml
	assert_failure
}

@test "aube update -r --latest: prerelease workspace mix preserves higher pins" {
	# Ported from pnpm/test/update.ts:728 ('update to latest recursive
	# workspace (outdated, updated, prerelease, outdated)'). Four
	# projects pinned at different versions; only the one already on
	# 3.0.0-rc.0 stays put — the rest bump to the 2.0.0 latest dist-tag.
	# Manifest assertions cover the recursive prerelease-preservation
	# behavior; shared lockfile placement is covered above.
	_require_registry

	add_dist_tag '@pnpm.e2e/has-prerelease' latest 2.0.0

	for i in 1 2 3 4; do
		mkdir project-$i
	done
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "1.0.0", "dependencies": { "@pnpm.e2e/has-prerelease": "1.0.0" } }
JSON
	cat >project-2/package.json <<'JSON'
{ "name": "project-2", "version": "1.0.0", "dependencies": { "@pnpm.e2e/has-prerelease": "2.0.0" } }
JSON
	cat >project-3/package.json <<'JSON'
{ "name": "project-3", "version": "1.0.0", "dependencies": { "@pnpm.e2e/has-prerelease": "3.0.0-rc.0" } }
JSON
	cat >project-4/package.json <<'JSON'
{ "name": "project-4", "version": "1.0.0", "dependencies": { "@pnpm.e2e/has-prerelease": "1.0.0" } }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
  - project-3
  - project-4
YAML

	run aube update -r --latest
	assert_success

	# Outdated pins bump to the new latest.
	run grep '"@pnpm.e2e/has-prerelease": "2.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/has-prerelease": "2.0.0"' project-2/package.json
	assert_success
	run grep '"@pnpm.e2e/has-prerelease": "2.0.0"' project-4/package.json
	assert_success

	# Prerelease pin survives — 3.0.0-rc.0 > 2.0.0.
	run grep '"@pnpm.e2e/has-prerelease": "3.0.0-rc.0"' project-3/package.json
	assert_success
}

@test "aube update -r --latest: prerelease + outdated workspace mix" {
	# Ported from pnpm/test/update.ts:807 ('update to latest recursive
	# workspace (prerelease, outdated)'). Two-project variant: one on
	# the prerelease (stays put), one on the older 1.0.0 (bumps).
	_require_registry

	add_dist_tag '@pnpm.e2e/has-prerelease' latest 2.0.0

	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "1.0.0", "dependencies": { "@pnpm.e2e/has-prerelease": "3.0.0-rc.0" } }
JSON
	cat >project-2/package.json <<'JSON'
{ "name": "project-2", "version": "1.0.0", "dependencies": { "@pnpm.e2e/has-prerelease": "1.0.0" } }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	run aube update -r --latest
	assert_success

	run grep '"@pnpm.e2e/has-prerelease": "3.0.0-rc.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/has-prerelease": "2.0.0"' project-2/package.json
	assert_success
}

@test "aube update <pkg>@latest: parses arg syntax, rewrites manifest past range" {
	# Ported from pnpm/test/update.ts:14 ('update <dep>'). pnpm uses
	# `pnpm update <pkg>@latest`; aube now parses the same arg syntax,
	# so this is a direct translation rather than the
	# `aube update --latest <pkg>` rewrite the earlier test in this
	# file uses.
	_require_registry

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-pkg-at-latest",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0'
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0

	run aube update '@pnpm.e2e/dep-of-pkg-with-1-dep@latest'
	assert_success

	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '"\^101.0.0"' package.json
	assert_success
}

@test "aube update <indirect-pkg>@latest: refreshes a transitive dep, leaves manifest alone" {
	# Ported from pnpm/test/update.ts:690 ('update indirect dependency
	# should not update package.json').
	#
	# Two-package fixture: `pkg-with-1-dep` is the only direct dep and it
	# transitively pulls in `dep-of-pkg-with-1-dep`. After bumping both
	# packages' dist-tags, `aube update <indirect>@latest` must:
	#   - Refresh the indirect to the new latest in the lockfile.
	#   - Leave the direct dep at its locked version (100.0.0) — even
	#     though latest is now 100.1.0, the user only asked to bump the
	#     indirect.
	#   - Leave package.json untouched (the indirect has no manifest
	#     entry to rewrite).
	_require_registry

	add_dist_tag '@pnpm.e2e/pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-indirect",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/pkg-with-1-dep": "^100.0.0"
  }
}
JSON

	run aube install
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	# Both packages get a new latest published. Without the indirect-arg
	# handling in update.rs, aube's `update -L` errored out at this point
	# with "package 'X' is not a dependency".
	add_dist_tag '@pnpm.e2e/pkg-with-1-dep' latest 100.1.0
	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.1.0

	run aube update '@pnpm.e2e/dep-of-pkg-with-1-dep@latest'
	assert_success

	# Indirect bumped to the new latest.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.1.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_failure

	# Direct dep stays at 100.0.0 — only the named arg should bump.
	run grep '@pnpm.e2e/pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/pkg-with-1-dep@100.1.0' aube-lock.yaml
	assert_failure

	# Manifest range untouched — the indirect has no manifest entry, so
	# the rewrite path skips it.
	run grep '"@pnpm.e2e/pkg-with-1-dep": "\^100.0.0"' package.json
	assert_success
}

@test "aube update --prod <devdep>: errors instead of silently bumping the dev dep" {
	# Regression for the cursor-bugbot finding on PR #446. A devDep named
	# under `--prod` (or a regular dep under `--dev`, an optional dep
	# under `--no-optional`) doesn't appear in `all_specifiers` and
	# previously slipped through the new indirect-dep path: the lockfile
	# graph has the entry regardless of bucket, `in_graph` passes, and
	# the dep silently re-resolves. The fix consults the unfiltered
	# direct-deps set first and errors before the indirect branch.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-flag-excluded",
  "version": "0.0.0",
  "devDependencies": {
    "@pnpm.e2e/foo": "100.0.0"
  }
}
JSON

	run aube install
	assert_success
	run grep '@pnpm.e2e/foo@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	run aube update --prod '@pnpm.e2e/foo'
	assert_failure
	assert_output --partial "is not a dependency"

	# Lockfile pin survives — the failed update must not leave the dep
	# silently re-resolved at the new latest.
	run grep '@pnpm.e2e/foo@100.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_failure
}

@test "aube update <pkg>@<spec>: rejects non-latest specs with a helpful error" {
	# Regression for the silent-spec-drop greptile flagged on PR #446.
	# `aube update foo@^2.0.0` used to be accepted at the arg-parse layer
	# but the spec was silently swallowed — the user got an in-range
	# refresh on `foo` instead of the range bump they typed. Now any
	# `@<spec>` other than `@latest` errors with a message pointing at
	# the supported syntax.
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-spec-reject",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON

	run aube update '@pnpm.e2e/foo@^200.0.0'
	assert_failure
	assert_output --partial "package spec '@pnpm.e2e/foo@^200.0.0' is not supported"
	assert_output --partial '--latest'
}

@test "aube update -r <indirect-pkg>@latest: refreshes the indirect across all projects" {
	# Regression for the silent-no-op greptile flagged on PR #446.
	# `aube update -r <indirect>@latest` used to bail at the per-project
	# filter because the indirect dep isn't in any project's `declared`
	# (direct deps), so `per_pkg.packages` was always empty and every
	# project was silently skipped. The fix consults each project's
	# lockfile (or falls back to the workspace-root shared one) so
	# transitive deps flow into the inner `run` call.
	_require_registry

	add_dist_tag '@pnpm.e2e/pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0

	cat >package.json <<'JSON'
{
  "name": "workspace-root",
  "version": "0.0.0",
  "private": true
}
JSON
	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pkg-with-1-dep": "^100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pkg-with-1-dep": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	# Seed via plain `install` so the shared workspace lockfile lands at
	# the root (not per-project) — mimics the most common workspace
	# starting point.
	run aube install
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.1.0

	run aube update -r '@pnpm.e2e/dep-of-pkg-with-1-dep@latest'
	assert_success

	# The shared lockfile bumped the transitive dep for both projects;
	# this would have stayed at 100.0.0 under the old silent-skip
	# behavior.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.1.0' aube-lock.yaml
	assert_success
	assert_file_not_exists project-1/aube-lock.yaml
	assert_file_not_exists project-2/aube-lock.yaml

	# Direct dep (the parent) stays at its locked version — only the
	# named indirect arg should bump.
	run grep '@pnpm.e2e/pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	# Manifests untouched — indirect deps have no manifest entry.
	run grep '"@pnpm.e2e/pkg-with-1-dep": "\^100.0.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/pkg-with-1-dep": "\^100.0.0"' project-2/package.json
	assert_success
}

@test "aube update <pkg>: rewrites caret manifest specifier to track in-range bump" {
	# Ported from pnpm/test/update.ts:51 ('update without --latest').
	# Default `updateRewritesSpecifier=true` makes the no-`--latest`
	# `aube update <pkg>` rewrite the caret manifest range to track the
	# new in-range max. Bumping `latest` to `100.1.0` keeps the new
	# version inside the existing `^100.0.0` range — this is the
	# cosmetic floor-bump case, not a `--latest` cross-major.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-rewrites-caret",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/foo@^100.0.0'
	assert_success
	run grep '@pnpm.e2e/foo@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	run aube update '@pnpm.e2e/foo'
	assert_success

	# Lockfile bumps to the new in-range max.
	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success

	# package.json caret tracks the new resolved version.
	run grep '"@pnpm.e2e/foo": "\^100.1.0"' package.json
	assert_success
}

@test "aube update <pkg>: opt-out via update-rewrites-specifier=false keeps manifest frozen" {
	# Companion to the rewrite test above. Setting
	# `update-rewrites-specifier=false` in `.npmrc` restores aube's
	# pre-parity frozen-manifest behavior: lockfile bumps in-range,
	# package.json caret stays at the user's original spec.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-rewrites-optout",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/foo@^100.0.0'
	assert_success
	run grep '@pnpm.e2e/foo@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	cat >>.npmrc <<'EOF'
update-rewrites-specifier=false
EOF

	run aube update '@pnpm.e2e/foo'
	assert_success

	# Lockfile still bumps to the new in-range max.
	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success

	# package.json range stays at the user's original spec.
	run grep '"@pnpm.e2e/foo": "\^100.0.0"' package.json
	assert_success
}

@test "aube update <pkg>: preserves dist-tag manifest specs (latest, next, beta)" {
	# Regression guard for the cosmetic-rewrite filter: dist-tags
	# ("latest", "next", "beta") must not be mistaken for caret specs.
	# `range_prefix` defaults to "^" for unknown shapes so the filter
	# checks the literal leading char (`^` or `~`) instead.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-dist-tag-preserved",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "latest"
  }
}
JSON

	run aube install
	assert_success
	run grep '@pnpm.e2e/foo@100.0.0' aube-lock.yaml
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	run aube update '@pnpm.e2e/foo'
	assert_success

	# Lockfile re-resolves to the new latest.
	run grep '@pnpm.e2e/foo@100.1.0' aube-lock.yaml
	assert_success

	# Manifest spec stays as the dist-tag string verbatim.
	run grep '"@pnpm.e2e/foo": "latest"' package.json
	assert_success
}

@test "aube update -r <pkg>: rewrites caret manifest in every workspace project" {
	# Ported from pnpm/test/update.ts:95 ('recursive update without
	# --latest'). Same cosmetic floor-bump as the single-project case,
	# fanned out across two workspace projects.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0

	cat >package.json <<'JSON'
{
  "name": "workspace-root",
  "version": "0.0.0",
  "private": true
}
JSON
	mkdir project-1 project-2
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >project-2/package.json <<'JSON'
{
  "name": "project-2",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "^100.0.0"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
  - project-2
YAML

	# Seed the workspace lockfile at 100.0.0 before the dist-tag bump
	# so the cosmetic floor-bump has a prior pin to advance from.
	run aube install
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	run aube update -r '@pnpm.e2e/foo'
	assert_success

	# Both per-project manifests rewritten to track the new resolved
	# version while staying inside the original caret range.
	run grep '"@pnpm.e2e/foo": "\^100.1.0"' project-1/package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "\^100.1.0"' project-2/package.json
	assert_success
}

@test "aube update -r --latest: honors workspace-root updateConfig.ignoreDependencies" {
	# Discussion #602: per-project `cwd` made the ignoreDependencies
	# lookup miss `pnpm-workspace.yaml` (which lives at the workspace
	# root), so an entry like `is-odd` would still get bumped past
	# its pin during recursive updates. Walking up to the workspace
	# root before reading the yaml fixes it.
	mkdir -p packages/a packages/b
	cat >package.json <<'JSON'
{"name":"root","version":"0.0.0","private":true}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
updateConfig:
  ignoreDependencies:
    - is-odd
YAML
	cat >packages/a/package.json <<'JSON'
{"name":"a","version":"1.0.0","dependencies":{"is-odd":"0.1.2","is-even":"0.1.0"}}
JSON
	cat >packages/b/package.json <<'JSON'
{"name":"b","version":"1.0.0","dependencies":{"is-odd":"0.1.2"}}
JSON

	run aube install
	assert_success

	run aube update -r --latest
	assert_success

	# is-odd was on the workspace ignore list — its pin must survive
	# both per-project manifests untouched. `b` is left in its
	# original compact-JSON form because aube never rewrites it; `a`
	# gets pretty-printed because the is-even bump touches it.
	run grep '"is-odd": *"0.1.2"' packages/a/package.json
	assert_success
	run grep '"is-odd": *"0.1.2"' packages/b/package.json
	assert_success
	# is-even isn't ignored, so it gets bumped past the pin.
	run grep '"is-even": *"1.0.0"' packages/a/package.json
	assert_success
}

@test "aube update -w retargets at the workspace root from a sub-package" {
	# Mirrors `pnpm -w update`: from a sub-package, `update -w` must
	# rewrite the root manifest (not the sub-package's). Without `-w`,
	# the in-package run resolves the sub-package's lockfile / writes
	# the sub-package's manifest.
	cat >package.json <<'JSON'
{"name":"root","version":"0.0.0","private":true,"dependencies":{"is-odd":"0.1.2"}}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/api
	cat >packages/api/package.json <<'JSON'
{"name":"api","version":"1.0.0","dependencies":{"is-even":"0.1.0"}}
JSON

	run aube install
	assert_success

	cd packages/api
	run aube update -w --latest
	assert_success

	# Root manifest's is-odd moved off the 0.1.2 pin (avoid hardcoding
	# the registry's current `latest` so the test survives future
	# fixture additions); sub-package's is-even must stay frozen
	# because we never targeted it.
	run grep -E '"is-odd": *"[^0]\.' ../../package.json
	assert_success
	run grep '"is-even": *"0.1.0"' package.json
	assert_success
}

@test "aube update -r -w: -r keeps per-package retargeting (regression)" {
	# `-w` retargets cwd at the workspace root inside `run`. `-r`
	# fans out to every workspace package and retargets cwd per
	# package via `retarget_cwd`; if `-w` survived the inner clone,
	# every iteration would re-retarget to the workspace root, lock
	# + rewrite the root manifest in a loop, and the per-package
	# lockfile merge would have nothing to pick up. `-r` plus `-w`
	# must be equivalent to plain `-r` (which already includes the
	# root in its sweep).
	cat >package.json <<'JSON'
{"name":"root","version":"0.0.0","private":true,"dependencies":{"is-odd":"0.1.2"}}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/api
	cat >packages/api/package.json <<'JSON'
{"name":"api","version":"1.0.0","dependencies":{"is-even":"0.1.0"}}
JSON

	run aube install
	assert_success

	run aube update -r -w --latest
	assert_success

	# Both manifests bumped past their pins — the per-package fanout
	# updated `api` correctly instead of being clobbered by the
	# workspace-root retarget on every iteration.
	run grep -E '"is-odd": *"[^0]\.' package.json
	assert_success
	run grep -E '"is-even": *"[^0]\.' packages/api/package.json
	assert_success
}
