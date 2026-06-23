#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube store --help lists every subcommand" {
	run aube store --help
	assert_success
	assert_output --partial "path"
	assert_output --partial "prune"
	assert_output --partial "status"
	assert_output --partial "add"
}

@test "aube store path defaults to \$XDG_DATA_HOME/aube/store/v1" {
	run aube store path
	assert_success
	# `aube store path` prints the store-version directory containing
	# both `files/` (CAS) and `index/` (cached indexes), matching the
	# granularity of `pnpm store path`. HOME is isolated to the test
	# temp dir and XDG_DATA_HOME points inside it, so the resolved
	# path must match exactly.
	assert_output "$XDG_DATA_HOME/aube/store/v1"
}

@test "aube store path honors store-dir from .npmrc and appends v1" {
	mkdir -p custom-store
	echo "store-dir=$PWD/custom-store" >.npmrc
	run aube store path
	assert_success
	# aube appends its own schema suffix (`v1`) to the user-supplied
	# store-dir. The suffix exists so the on-disk layout is stable
	# across versions of aube and never collides with a pnpm store
	# rooted at the same path.
	assert_output "$PWD/custom-store/v1"
}

@test "aube store path honors storeDir from pnpm-workspace.yaml" {
	mkdir -p ws-store
	cat >pnpm-workspace.yaml <<EOF
storeDir: $PWD/ws-store
EOF
	run aube store path
	assert_success
	assert_output "$PWD/ws-store/v1"
}

@test "aube store path expands ~ in store-dir to \$HOME" {
	echo 'store-dir=~/custom-home-store' >.npmrc
	run aube store path
	assert_success
	assert_output "$HOME/custom-home-store/v1"
}

@test "aube store add fetches a package and subsequent install is warm" {
	# Pre-warm the store with is-odd; then the basic fixture install should
	# find it cached and fetch only the missing packages.
	run aube store add is-odd@3.0.1
	assert_success
	assert_output --partial "is-odd@3.0.1"

	# The cached index should exist for the added package. The
	# on-disk layout is `$STORE_V1/index/<16 hex>/<name>@<ver>.json`,
	# where the store-version directory is what `aube store path` prints.
	store_v1="$(aube store path)"
	run bash -c "compgen -G \"$store_v1/index/*/is-odd@3.0.1.json\""
	assert_success

	# Also sanity-check `store status` returns clean after an add.
	run aube store status
	assert_success
	assert_output --partial "consistent"
}

@test "aube store add rejects unknown packages" {
	run aube store add this-package-does-not-exist-xyz
	assert_failure
	assert_output --partial "not found"
}

@test "aube store status detects a corrupted file" {
	run aube store add is-odd@3.0.1
	assert_success

	# Pick one of the files the cached index points at and corrupt it.
	# Integrity-keyed entries live at
	# `<store_v1>/index/<16 hex>/<name>@<ver>.json` — walk two levels
	# to find the actual file.
	store_v1="$(aube store path)"
	index="$(find "$store_v1/index" -mindepth 2 -maxdepth 2 -name 'is-odd@3.0.1.json' -print -quit)"
	assert_file_exists "$index"
	store_path="$(grep -o '"store_path":"[^"]*"' "$index" | head -n1 | sed 's/.*":"//;s/"$//')"
	echo "garbage" >"$store_path"

	run aube store status
	assert_failure
	assert_output --partial "corrupt"
}

@test "aube store prune runs cleanly on an empty store" {
	run aube store prune
	assert_success
	assert_output --partial "empty"
}

@test "aube store prune actually deletes unreferenced files" {
	run aube store add is-odd@3.0.1
	assert_success

	# Drop the cached index so every file the `add` just wrote becomes
	# unreferenced. Without this the prune loop would `continue` on every
	# file and never exercise the deletion branch. Integrity-keyed
	# files live under `<store_v1>/index/<16 hex>/<name>@<ver>.json` —
	# glob the whole subdir layout.
	store_v1="$(aube store path)"
	rm "$store_v1/index"/*/is-odd@3.0.1.json

	run aube store prune
	assert_success
	assert_output --partial "Pruned"
	refute_output --partial "Pruned 0 files"
}
