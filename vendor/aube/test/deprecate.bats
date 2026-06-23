#!/usr/bin/env bats
#
# These tests are not parallel-safe: they PUT modified packuments to the
# shared Verdaccio fixture and restore the committed storage files via
# `git checkout` in teardown. Other tests that resolve `is-odd` or
# `is-number` concurrently would see mid-mutation state, and parallel
# `git checkout` invocations on the same paths race. Tag the file as
# serial and disable within-file parallelism so a `bats --jobs N` run
# can schedule it outside the parallel pool.
#
# bats file_tags=serial

# Force within-file tests to run one at a time regardless of --jobs.
# bats reads this variable from the file's scope after sourcing.
# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	# These tests mutate the committed Verdaccio storage (they PUT
	# modified packuments back to the local registry). Restore any
	# touched packuments from git so the fixture stays clean across
	# runs and so CI doesn't see a dirty working tree.
	if [ -n "${PROJECT_ROOT:-}" ]; then
		git -C "$PROJECT_ROOT" checkout -- \
			test/registry/storage/is-odd/package.json \
			test/registry/storage/is-number/package.json 2>/dev/null || true
	fi
	_common_teardown
}

# Skip if no local registry — these tests need to PUT a packument back.
_require_registry() {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

# Fetch a version's `deprecated` field from the registry. Returns the
# literal string "null" when absent so the caller can refute it.
_deprecated_field() {
	local name="$1" version="$2"
	curl -sf "${AUBE_TEST_REGISTRY}/${name}" |
		node -e "
		let d='';process.stdin.on('data',c=>d+=c).on('end',()=>{
			let p=JSON.parse(d);
			let v=p.versions&&p.versions['${version}'];
			process.stdout.write(v&&v.deprecated!=null?v.deprecated:'null');
		})
		"
}

@test "aube deprecate marks a matching version as deprecated" {
	_require_registry
	run aube deprecate 'is-odd@3.0.1' 'please use is-even'
	assert_success
	assert_output --partial 'Deprecated 1 version of is-odd'
	assert_output --partial '3.0.1'

	run _deprecated_field is-odd 3.0.1
	assert_output 'please use is-even'
}

@test "aube undeprecate clears an existing deprecation" {
	_require_registry
	run aube deprecate 'is-odd@0.1.2' 'temp message'
	assert_success

	run aube undeprecate 'is-odd@0.1.2'
	assert_success
	assert_output --partial 'Undeprecated 1 version of is-odd'

	# After undeprecate the field is either absent or the empty string,
	# depending on what the registry does with `deprecated: ""`. Both
	# mean "not deprecated" to every installer. Verdaccio normalizes to
	# absent; npm's public registry keeps the empty string. We accept
	# either rather than binding the test to one implementation.
	# macOS's BSD regex rejects empty alternation, so use `(null)?` rather
	# than `(null|)` for the "absent or empty" assertion.
	run _deprecated_field is-odd 0.1.2
	assert_output --regexp '^(null)?$'
}

@test "aube deprecate --dry-run does not modify the registry" {
	_require_registry
	run aube deprecate --dry-run 'is-number@3.0.0' 'noop'
	assert_success
	assert_output --partial 'Would deprecate 1 version of is-number'

	run _deprecated_field is-number 3.0.0
	assert_output 'null'
}

@test "aube deprecate errors when no version matches the range" {
	_require_registry
	run aube deprecate 'is-odd@99.0.0' 'nope'
	assert_failure
	assert_output --partial 'no published versions'
}

@test "aube deprecate errors on an unknown package" {
	_require_registry
	run aube deprecate 'this-package-does-not-exist-xyz' 'nope'
	assert_failure
	assert_output --partial 'package not found'
}
