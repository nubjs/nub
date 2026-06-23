#!/usr/bin/env bash

_common_setup() {
	load 'test_helper/bats-support/load'
	load 'test_helper/bats-assert/load'
	load 'test_helper/bats-file/load'

	PROJECT_ROOT="$(cd "$BATS_TEST_DIRNAME/.." && pwd)"
	export PATH="$PROJECT_ROOT/target/debug:$PATH"

	# Ensure the multicall shims (`aubr`, `aubx`) exist alongside `aube`.
	# Local `cargo build` produces all three as real binaries, but CI only
	# uploads `target/debug/aube` as an artifact; the bats shards then
	# download just that one file. Materialize the shims as hardlinks to
	# the shared `aube` inode so the argv[0] dispatch in `main.rs` resolves
	# correctly. `ln -f` is idempotent — it refreshes if `aube` was rebuilt
	# and is a no-op if the hardlinks already point at the same inode.
	local _aube_bin="$PROJECT_ROOT/target/debug/aube"
	if [ -x "$_aube_bin" ]; then
		ln -f "$_aube_bin" "$PROJECT_ROOT/target/debug/aubr" 2>/dev/null || true
		ln -f "$_aube_bin" "$PROJECT_ROOT/target/debug/aubx" 2>/dev/null || true
	fi

	TEST_TEMP_DIR="$(temp_make)"
	cd "$TEST_TEMP_DIR" || exit 1

	# Isolate HOME so we don't pollute the real pnpm store
	export HOME="$TEST_TEMP_DIR"
	export XDG_CACHE_HOME="$HOME/.cache"
	export XDG_CONFIG_HOME="$HOME/.config"
	export XDG_DATA_HOME="$HOME/.local/share"
	export XDG_STATE_HOME="$HOME/.local/state"
	export TMPDIR="$TEST_TEMP_DIR/tmp"
	mkdir -p "$XDG_CACHE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$TMPDIR"

	# Tests assume a "local dev" install environment. Unset CI so the
	# default FrozenMode is Prefer, not Frozen — otherwise tests that
	# install without a pre-built lockfile (e.g. workspace fixtures)
	# would fail with a frozen-lockfile error in CI runs.
	unset CI

	# Keep host shell logging preferences from overriding tests that assert
	# aube's CLI/config loglevel behavior.
	unset AUBE_LOG
	unset AUBE_DEBUG
	unset AUBE_TRACE

	# Keep the update notifier (install.rs:… -> update_check.rs) from
	# hitting aube.jdx.dev during BATS. Unsetting CI re-enables it by
	# default, so we suppress it explicitly here — otherwise every
	# `aube install` / `add` / `update` test would spend ~1.5s on a
	# DNS/timeout round-trip for no benefit.
	export AUBE_NO_UPDATE_CHECK=1

	# Force plain-text output across all bats invocations. GitHub
	# Actions and other CI runners auto-enable ANSI on aube even
	# without a TTY (the log viewer renders escape codes correctly),
	# but `assert_output --partial "..."` asserts on raw bytes — so
	# any styled label/value that splits ANSI mid-substring stops
	# matching once color is on. Tests that exercise color logic
	# (color.bats, guardrails.bats, settings.bats) `unset NO_COLOR`
	# locally before running aube, so this default is non-disruptive.
	export NO_COLOR=1

	# Point to local Verdaccio registry if running
	if [ -n "${AUBE_TEST_REGISTRY:-}" ]; then
		echo "registry=${AUBE_TEST_REGISTRY}" >"$TEST_TEMP_DIR/.npmrc"
	fi

	# `aube add` runs two supply-chain gates against public APIs
	# (api.osv.dev for MAL-* advisories, api.npmjs.org for weekly
	# downloads). Bats tests target the local Verdaccio mirror and
	# can't depend on outbound network reachability, so disable
	# both gates wholesale. Tests that specifically exercise the
	# gates can re-enable them in-test.
	export AUBE_ADVISORY_CHECK=off
	export AUBE_LOW_DOWNLOAD_THRESHOLD=0
}

_common_teardown() {
	temp_del "$TEST_TEMP_DIR"
}

# Create a minimal package.json + aube-lock.yaml fixture in cwd.
# Deliberately does NOT copy the pnpm-lock.yaml sidecar — tests that
# want to exercise the pnpm→aube migration path should copy it
# explicitly.
_setup_basic_fixture() {
	cp "$PROJECT_ROOT/fixtures/basic/package.json" .
	cp "$PROJECT_ROOT/fixtures/basic/aube-lock.yaml" .
}

# Set a dist-tag on a package in the committed Verdaccio storage.
# Verdaccio re-reads package.json from disk per request, so the new
# tag takes effect on the next resolve without a registry restart.
#
# Mutates test/registry/storage/<pkg>/package.json — callers MUST
# restore the file via `git checkout` in teardown (see deprecate.bats
# for the canonical pattern) so the fixture stays clean across runs.
# Tests using this helper are not parallel-safe and should set
# `# bats file_tags=serial` + `BATS_NO_PARALLELIZE_WITHIN_FILE=1`.
#
# Usage: add_dist_tag <pkg> <tag> <version>
add_dist_tag() {
	local pkg="$1" tag="$2" version="$3"
	local file="$PROJECT_ROOT/test/registry/storage/${pkg}/package.json"
	local tmp="${file}.tmp"
	jq --arg t "$tag" --arg v "$version" '.["dist-tags"][$t] = $v' "$file" >"$tmp" || {
		rm -f "$tmp"
		return 1
	}
	mv "$tmp" "$file"
}
