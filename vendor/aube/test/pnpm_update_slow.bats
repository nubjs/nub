#!/usr/bin/env bats
#
# Network-dependent ports of pnpm/test/update.ts. These exercise paths
# that hit real upstream services (github.com codeload for git deps),
# which the offline Verdaccio fixture can't host.
#
# Gated behind AUBE_NETWORK_TESTS=1 so the default `mise run test:bats`
# stays offline. CI opts in by setting the env var explicitly.
#
# Mirrors the dist-tag mutation pattern of test/pnpm_update.bats —
# tagged serial and parallel-disabled within the file.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	if [ -n "${PROJECT_ROOT:-}" ]; then
		git -C "$PROJECT_ROOT" checkout -- \
			test/registry/storage/@pnpm.e2e/bar/package.json \
			test/registry/storage/@pnpm.e2e/dep-of-pkg-with-1-dep/package.json \
			test/registry/storage/@pnpm.e2e/foo/package.json \
			test/registry/storage/@pnpm.e2e/qar/package.json 2>/dev/null || true
	fi
	_common_teardown
}

_require_registry() {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

_require_network() {
	if [ "${AUBE_NETWORK_TESTS:-}" != "1" ]; then
		skip "set AUBE_NETWORK_TESTS=1 to run network tests"
	fi
}

@test "aube update --latest preserves bare github shorthand alongside registry deps" {
	# Ported from pnpm/test/update.ts:143 ('update --latest') with the
	# `kevva/is-negative` GitHub-shorthand assertion restored.
	#
	# Regression guard: aube_lockfile::parse_git_spec recognizes bare
	# `user/repo`, the resolver routes it through the git path, and
	# `aube update --latest` skips non-registry specs in the manifest
	# rewrite (otherwise the bare shorthand would silently become a
	# semver range pin and break install).
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-with-github",
  "version": "0.0.0",
  "dependencies": {
    "is-negative": "kevva/is-negative"
  }
}
JSON

	# Install the github dep first so the lockfile has it, then add the
	# registry deps. Installing through `aube add` would fail today
	# because the CLI add path doesn't recognize bare shorthand —
	# tracked as a separate feature.
	run aube install
	assert_success

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0' '@pnpm.e2e/bar@^100.0.0' 'alias@npm:@pnpm.e2e/qar@^100.0.0'
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update --latest
	assert_success

	# Registry deps bumped past their original ranges.
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '@pnpm.e2e/bar@100.1.0' aube-lock.yaml
	assert_success
	run grep 'alias@100.1.0' aube-lock.yaml
	assert_success

	# Manifest specs tracked the new versions, preserving caret + alias.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "\^101.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@\^100.1.0"' package.json
	assert_success

	# The github shorthand survives `update --latest` untouched —
	# parse_git_spec recognizes the bare form, the rewrite branch skips
	# it, the lockfile retains the resolved git source.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}

@test "aube add kevva/is-negative installs github shorthand end-to-end" {
	# Ported from pnpm/test/update.ts:14 ('update <dep>') with the
	# `kevva/is-negative` GitHub-shorthand assertion restored end-to-end
	# through `aube add` (instead of pre-writing the dep into
	# package.json before `aube install`).
	#
	# Regression guard for the CLI add path: parse_pkg_spec routes the
	# bare shorthand through the git branch, the packument fetch is
	# skipped, the verbatim spec is written to package.json, and the
	# install pipeline resolves it via the resolver's git path. After a
	# registry-side dist-tag bump and `update --latest`, the github
	# shorthand survives untouched in the manifest.
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-add-github",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@^100.0.0' kevva/is-negative
	assert_success
	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0

	run aube update --latest '@pnpm.e2e/dep-of-pkg-with-1-dep'
	assert_success

	run grep '@pnpm.e2e/dep-of-pkg-with-1-dep@101.0.0' aube-lock.yaml
	assert_success
	run grep '"\^101.0.0"' package.json
	assert_success

	# The github shorthand survives — git specs aren't bumped.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}

@test "aube add kevva/is-negative + update --latest --save-exact preserves shorthand" {
	# Ported from pnpm/test/update.ts:170 ('update --latest --save-exact')
	# with the `kevva/is-negative` GitHub-shorthand assertion restored
	# end-to-end through `aube add`.
	#
	# Regression guard: --save-exact rewrites caret-pinned registry deps
	# to exact versions but leaves git specs untouched.
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-exact-with-github",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' '@pnpm.e2e/bar@100.0.0' 'alias@npm:@pnpm.e2e/qar@100.0.0' kevva/is-negative
	assert_success
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update --latest -E
	assert_success

	# Registry deps rewritten as exact pins, npm: alias preserved.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "101.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/bar": "100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@100.1.0"' package.json
	assert_success

	# The github shorthand survives — git specs aren't bumped.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}

@test "aube add kevva/is-negative + named update leaves shorthand alone" {
	# Ported from pnpm/test/update.ts:197 ('update --latest specific
	# dependency') with the `kevva/is-negative` GitHub-shorthand
	# assertion restored end-to-end through `aube add`.
	#
	# Regression guard: named `update --latest <pkg>` only touches the
	# specified registry deps; git specs and unnamed registry deps
	# stay at their original pins.
	_require_registry
	_require_network

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.0.0
	cat >package.json <<'JSON'
{
  "name": "pnpm-update-latest-named-with-github",
  "version": "0.0.0"
}
JSON

	run aube add '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' '@pnpm.e2e/bar@^100.0.0' '@pnpm.e2e/foo@100.0.0' 'alias@npm:@pnpm.e2e/qar@^100.0.0' kevva/is-negative
	assert_success

	add_dist_tag '@pnpm.e2e/dep-of-pkg-with-1-dep' latest 101.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	add_dist_tag '@pnpm.e2e/qar' latest 100.1.0

	run aube update -L '@pnpm.e2e/bar' alias
	assert_success

	# Named registry deps bumped.
	run grep '"@pnpm.e2e/bar": "\^100.1.0"' package.json
	assert_success
	run grep '"alias": "npm:@pnpm.e2e/qar@\^100.1.0"' package.json
	assert_success

	# Unnamed registry deps stay at their original pins.
	run grep '"@pnpm.e2e/dep-of-pkg-with-1-dep": "100.0.0"' package.json
	assert_success
	run grep '"@pnpm.e2e/foo": "100.0.0"' package.json
	assert_success

	# The github shorthand survives untouched.
	run grep '"is-negative": "kevva/is-negative"' package.json
	assert_success
}
