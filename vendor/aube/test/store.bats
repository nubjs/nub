#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
	export AUBE_GLOBAL_VIRTUAL_STORE_DIR="$TEST_TEMP_DIR/global-virtual-store"
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
	[ ! -e "$XDG_DATA_HOME/aube/store/v1" ]
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

@test "aube store maintenance fails closed on a malformed package index" {
	run aube store add is-odd@3.0.1
	assert_success

	store_v1="$(aube store path)"
	index="$(find "$store_v1/index" -mindepth 2 -maxdepth 2 -name 'is-odd@3.0.1.json' -print -quit)"
	assert_file_exists "$index"
	before="$(find "$store_v1/files" -type f | wc -l)"
	echo '{not valid JSON' >"$index"

	run aube store prune --dry-run
	assert_failure
	assert_output --partial "ERR_AUBE_STORE_INDEX_SCAN_FAILED"
	assert_output --partial "is-odd@3.0.1.json"
	assert_equal "$before" "$(find "$store_v1/files" -type f | wc -l)"

	run aube store prune
	assert_failure
	assert_output --partial "ERR_AUBE_STORE_INDEX_SCAN_FAILED"
	assert_output --partial "is-odd@3.0.1.json"
	assert_equal "$before" "$(find "$store_v1/files" -type f | wc -l)"

	run aube store status
	assert_failure
	assert_output --partial "ERR_AUBE_STORE_INDEX_SCAN_FAILED"
	assert_output --partial "is-odd@3.0.1.json"
}

@test "aube store prune preserves files when an index path is unreadable" {
	run aube store add is-odd@3.0.1
	assert_success

	store_v1="$(aube store path)"
	index="$(find "$store_v1/index" -mindepth 2 -maxdepth 2 -name 'is-odd@3.0.1.json' -print -quit)"
	assert_file_exists "$index"
	index_dir="$(dirname "$index")"
	before="$(find "$store_v1/files" -type f | wc -l)"
	chmod 000 "$index"

	# Root can bypass mode bits, so skip rather than asserting a false
	# negative in privileged containers.
	if test -r "$index"; then
		chmod 600 "$index"
		skip "running as a user that can bypass file permissions"
	fi

	run aube store prune --dry-run
	chmod 600 "$index"
	assert_failure
	assert_output --partial "ERR_AUBE_STORE_INDEX_SCAN_FAILED"
	assert_output --partial "is-odd@3.0.1.json"
	assert_equal "$before" "$(find "$store_v1/files" -type f | wc -l)"

	chmod 000 "$index_dir"

	if test -r "$index"; then
		chmod 700 "$index_dir"
		skip "running as a user that can bypass directory permissions"
	fi

	run aube store prune
	chmod 700 "$index_dir"
	assert_failure
	assert_output --partial "ERR_AUBE_STORE_INDEX_SCAN_FAILED"
	assert_output --partial "$(basename "$index_dir")"
	assert_equal "$before" "$(find "$store_v1/files" -type f | wc -l)"
}

@test "aube store prune runs cleanly on an empty store" {
	run aube store prune
	assert_success
	assert_output --partial "Nothing to prune"
	[ ! -d "$AUBE_GLOBAL_VIRTUAL_STORE_DIR/v1" ]
}

@test "aube store prune does not call a populated store empty" {
	run aube store add is-odd@3.0.1
	assert_success

	run aube store prune
	assert_success
	assert_output --partial "Nothing to prune"
	refute_output --partial "empty"
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

@test "aube store prune removes orphaned streamed-entry tempfiles" {
	store_v1="$(aube store path)"
	mkdir -p "$store_v1/files"
	stream_temp="$store_v1/files/.aube-stream-orphan"
	dd if=/dev/zero of="$stream_temp" bs=1024 count=4 2>/dev/null

	run aube store prune --dry-run
	assert_success
	assert_file_exists "$stream_temp"
	assert_output --partial "Would prune 1 file"

	run aube store prune
	assert_success
	assert_file_not_exists "$stream_temp"
	assert_output --partial "Pruned 1 file"
	refute_output --partial "up to"
}

@test "aube store prune removes entries from deleted registered projects" {
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "gvs-prune-project",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	run bash -c 'cd project && aube install'
	assert_success

	gvs="$AUBE_GLOBAL_VIRTUAL_STORE_DIR/v1"
	legacy="$AUBE_GLOBAL_VIRTUAL_STORE_DIR/legacy@1.0.0-deadbeefdeadbeef"
	mkdir -p "$legacy"
	assert_dir_exists "$gvs"
	assert [ -n "$(find "$gvs" -mindepth 1 -maxdepth 1 -type d ! -name node_modules ! -name '.*' -print -quit)" ]
	# A warm install must restore missing registration without forcing the
	# linker to run again.
	rm -rf "$gvs/.projects"
	run bash -c 'cd project && aube install'
	assert_success
	assert_output --partial "Already up to date"
	assert_dir_exists "$gvs/.projects"
	rm -rf project

	run aube store prune --dry-run
	assert_success
	assert_output --partial "Would prune"
	assert_output --partial "from the global virtual store"
	assert [ -n "$(find "$gvs" -mindepth 1 -maxdepth 1 -type d ! -name node_modules ! -name '.*' -print -quit)" ]

	run aube store prune
	assert_success
	assert_output --partial "from the global virtual store"
	assert [ -z "$(find "$gvs" -mindepth 1 -maxdepth 1 -type d ! -name node_modules ! -name '.*' -print -quit)" ]
	assert [ -z "$(find "$gvs/.projects" -mindepth 1 -maxdepth 1 -type f -print -quit)" ]
	assert_dir_exists "$legacy"
}

@test "GVS registration failure does not report install success" {
	mkdir project
	cat >project/package.json <<'JSON'
{
  "name": "gvs-registration-failure",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	run bash -c 'cd project && aube install'
	assert_success

	chmod a-w "$AUBE_GLOBAL_VIRTUAL_STORE_DIR/v1/.projects"
	run bash -c 'cd project && aube install'
	chmod u+w "$AUBE_GLOBAL_VIRTUAL_STORE_DIR/v1/.projects"
	assert_failure
	refute_output --partial "Already up to date"
}

@test "aube store prune --dry-run reports candidates without deleting them" {
	run aube store add is-odd@3.0.1
	assert_success

	# Same setup as the deleting test above: drop the cached index so the
	# files `add` just wrote become unreferenced prune candidates.
	store_v1="$(aube store path)"
	rm "$store_v1/index"/*/is-odd@3.0.1.json

	before="$(find "$store_v1/files" -type f | wc -l)"

	run aube store prune --dry-run
	assert_success
	assert_output --partial "Would prune"
	refute_output --partial "Would prune 0 files"

	# Nothing left the store, so the same candidates are still on disk for
	# a real prune to remove.
	after="$(find "$store_v1/files" -type f | wc -l)"
	assert_equal "$before" "$after"

	run aube store prune
	assert_success
	assert_output --partial "Pruned"
	refute_output --partial "Pruned 0 files"
}

@test "aube store prune --dry-run on an empty store" {
	run aube store prune --dry-run
	assert_success
	assert_output --partial "Nothing to prune"
}

@test "aube store prune --json requires --dry-run" {
	run aube store prune --json
	assert_failure
	assert_output --partial "--dry-run"
}

@test "aube store prune retains files referenced only by a legacy index" {
	run aube store add is-odd@3.0.1
	assert_success

	store_v1="$(aube store path)"
	current_index="$(find "$store_v1/index" -name 'is-odd@3.0.1.json' -print -quit)"
	legacy_index="$XDG_CACHE_HOME/aube/index/legacy/is-odd@3.0.1.json"
	mkdir -p "$(dirname "$legacy_index")"
	cp "$current_index" "$legacy_index"
	legacy_store_path="$(grep -o '"store_path":"[^"]*"' "$legacy_index" | head -n1 | sed 's/.*":"//;s/"$//')"
	rm "$current_index"

	# Keep the current index directory populated too, reproducing the state
	# where an interrupted or partial migration left live records in both.
	run aube store add is-even@1.0.0
	assert_success
	assert_file_exists "$legacy_store_path"

	run aube store prune
	assert_success
	assert_file_exists "$legacy_store_path"
}

@test "aube store prune JSON reports every root and leaves legacy indexes unmigrated" {
	legacy="$XDG_CACHE_HOME/aube/index"
	mkdir -p "$legacy"
	echo '{}' >"$legacy/legacy@1.0.0.json"

	run aube store prune --dry-run --json
	assert_success
	echo "$output" | jq -e '
		.schemaVersion == 1 and
		.dryRun == true and
		([.mutationRoots[].kind] | index("contentStore") != null) and
		([.mutationRoots[].kind] | index("globalVirtualStore") != null) and
		([.mutationRoots[].kind] | index("extractedTrees") != null) and
		([.mutationRoots[].kind] | index("legacyPackageIndex") != null) and
		([.actions[].kind] | index("migrateLegacyPackageIndex") != null) and
		([.actions[].kind] | index("pruneExtractedTreeEntries") != null) and
		.extractedTrees == {
			"entries": 0,
			"bytesUpperBound": 0,
			"deferredEntries": 0,
			"skippedNoProjects": false
		}
	' >/dev/null
	assert_file_exists "$legacy/legacy@1.0.0.json"
	[ ! -d "$XDG_DATA_HOME/aube/store/v1/index" ]
}

@test "aube store prune JSON models GVS hardlink removal before CAS pruning" {
	store_v1="$(aube store path)"
	cas="$store_v1/files/aa"
	gvs="$AUBE_GLOBAL_VIRTUAL_STORE_DIR/v1"
	mkdir -p "$cas" "$gvs/.projects" "$gvs/orphan/node_modules/pkg"
	content="$cas/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	printf 'shared-content' >"$content"
	ln "$content" "$gvs/orphan/node_modules/pkg/index.js"

	run aube store prune --dry-run --json
	assert_success
	echo "$output" | jq -e '
		.globalVirtualStore.entries == 1 and
		.contentStore.files == 1 and
		.globalVirtualStore.bytesUpperBound == 14 and
		.contentStore.bytesUpperBound == 14 and
		.reclaimableBytesUpperBound == 14
	' >/dev/null
	assert_file_exists "$content"
	assert_dir_exists "$gvs/orphan"

	run aube store prune
	assert_success
	[ ! -f "$content" ]
	[ ! -d "$gvs/orphan" ]
}
