#!/usr/bin/env bats
#
# Ported from pnpm/test/install/preferOffline.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Coverage focus: --prefer-offline reuses cached packument metadata
# even when the registry has published a newer dist-tag since the
# cache was populated. Mutates dist-tags on the committed Verdaccio
# storage via add_dist_tag and restores them via git checkout in
# teardown — same pattern as test/deprecate.bats / test/pnpm_update.bats.
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
			test/registry/storage/@pnpm.e2e/foo/package.json 2>/dev/null || true
	fi
	_common_teardown
}

_require_registry() {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

@test "aube install --prefer-offline: reuses cached metadata, ignoring a newer registry latest" {
	# Ported from pnpm/test/install/preferOffline.ts:11
	# ('when prefer offline is used, meta from store is used, where
	#  latest might be out-of-date').
	#
	# Substitution: pnpm's `pnpm install @pnpm.e2e/foo` (overloaded
	# install/add) → declare the dep in package.json with spec "latest"
	# and run `aube install`. aube splits install/add, and using the
	# "latest" tag spec makes the second resolve depend on the cached
	# packument's `dist-tags.latest`, which is what `--prefer-offline`
	# pins in place. Same regression guard: the second install must NOT
	# revalidate against the registry's newer latest.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0

	cat >package.json <<'JSON'
{
  "name": "pnpm-prefer-offline",
  "version": "0.0.0",
  "dependencies": {
    "@pnpm.e2e/foo": "latest"
  }
}
JSON

	# First install: populates the packument cache with latest=100.0.0.
	run aube install
	assert_success
	run cat node_modules/@pnpm.e2e/foo/package.json
	assert_success
	assert_output --partial '"version": "100.0.0"'

	# Wipe the install state so the second resolve has to rerun (not
	# short-circuit through the frozen-lockfile fast path).
	rm -rf node_modules aube-lock.yaml

	# Registry now publishes 100.1.0 as latest. Without --prefer-offline
	# the resolver would revalidate the packument and pick this up.
	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0

	run aube install --prefer-offline
	assert_success

	# The cached packument still pointed at 100.0.0, so that's what
	# resolves — even though the registry has moved on.
	run cat node_modules/@pnpm.e2e/foo/package.json
	assert_success
	assert_output --partial '"version": "100.0.0"'
}
