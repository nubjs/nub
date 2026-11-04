#!/usr/bin/env bats
#
# Regressions for the fetch phase's already-linked shortcut and the
# lockfile freshness fast path.
#
# The shortcut classifies a package as `AlreadyLinked` (skipping the
# store index load) whenever its `node_modules/.aube/<dep>` entry
# resolves. Workspaces used to opt out of it entirely, paying a
# stat-per-store-file on every install. These tests pin the safety
# properties that make opting back in correct, so a future change that
# breaks them fails loudly instead of silently shipping a stale tree.
#
# The freshness tests drive `aube run`, because `ensure_installed` is
# what actually calls `check_needs_install` — a bare `aube install`
# reports "Already up to date" whenever the pipeline finds nothing to
# do, which would pass whether or not the freshness check ran.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_ws_fixture() {
	cat >package.json <<'JSON'
{
  "name": "ws-root",
  "version": "1.0.0",
  "private": true
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/a
	cat >packages/a/package.json <<'JSON'
{
  "name": "a",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
}

# Read a dependency's installed manifest *through* the symlink chain, so
# the assertion covers the whole `importer -> .aube -> store` path rather
# than just the presence of a link.
_assert_installed_is_odd() {
	run cat packages/a/node_modules/is-odd/package.json
	assert_success
	assert_output --partial '"is-odd"'
	assert_output --partial '3.0.1'
}

_setup_single_fixture() {
	cat >package.json <<'JSON'
{
  "name": "single-root",
  "version": "1.0.0",
  "private": true,
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
}

# Single-project counterpart of `_assert_installed_is_odd`: the root
# importer's own `node_modules/` is the tree under test.
_assert_installed_is_odd_root() {
	run cat node_modules/is-odd/package.json
	assert_success
	assert_output --partial '"is-odd"'
	assert_output --partial '3.0.1'
}

@test "workspace install rebuilds a removed virtual store entry" {
	# The shortcut keys off `Path::exists`, which follows symlinks, so a
	# `.aube/<dep>` entry whose target is gone must NOT classify as
	# `AlreadyLinked` — it has to fall through to the store and
	# re-materialize. Without that, a wiped global virtual store would
	# leave the workspace tree permanently broken.
	_setup_ws_fixture

	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd

	entry="$(find node_modules/.aube -maxdepth 1 -name 'is-odd@*' -print -quit)"
	[ -n "$entry" ]
	rm -rf "$entry"

	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd
	restored="$(find node_modules/.aube -maxdepth 1 -name 'is-odd@*' -print -quit)"
	[ -n "$restored" ]
	_assert_installed_is_odd
}

@test "workspace install re-materializes a dangling virtual store link" {
	# The sharper form of the test above, and the property the whole
	# shortcut rests on: the `.aube/<dep>` symlink is still present, but
	# the global virtual store directory it points at is gone. The
	# existence probe follows symlinks, so this must classify as a miss
	# and rebuild — if it ever regressed to an lstat, the install would
	# "succeed" while leaving every dependent pointing into thin air.
	#
	# The target is resolved through the link rather than hardcoded, so
	# this keeps working if the global virtual store's default location
	# moves.
	_setup_ws_fixture

	run aube install
	assert_success

	entry="$(find node_modules/.aube -maxdepth 1 -name 'is-odd@*' -print -quit)"
	[ -n "$entry" ]
	[ -L "$entry" ]
	target="$(readlink -f "$entry")"
	[ -n "$target" ]
	rm -rf "${target:?}"
	# The link is now dangling: present to lstat, absent to stat.
	[ -L "$entry" ]
	[ ! -e "$entry" ]

	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd
	_assert_installed_is_odd
}

@test "workspace install keeps an intact tree usable after the CAS is wiped" {
	# The flip side of the test above: when the entries DO resolve, the
	# materialized tree is self-sufficient (its files are hardlinks or
	# copies that outlive their CAS shards), so wiping the store must
	# not break the install. This is the property that the skipped
	# `load_index_verified` call was never protecting.
	_setup_ws_fixture

	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd

	store_v1="$(aube store path)"
	rm -rf "${store_v1:?}/files"

	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd
	_assert_installed_is_odd
}

@test "workspace install self-heals a stale index when the entry resolves elsewhere" {
	# The case that keeps `skip_already_linked_shortcut` on for workspace
	# installs. The local `.aube/<dep>` name is keyed by dep_path alone
	# while the virtual-store subdir folds in graph hashes, so an entry
	# can keep resolving while the target the graph expects moves. If the
	# fetch phase skipped verification for such a package, the linker
	# would need an index it was never handed and fall back to the
	# *unverified* `store.load_index` — returning a stale index whose CAS
	# shards are gone and failing the link, with no way to re-fetch.
	#
	# Verifying at fetch time keeps this self-healing: the stale index
	# drops to `NeedsFetch`, the tarball is re-downloaded, and the store,
	# the expected target and the link are all restored.
	_setup_ws_fixture

	run aube install
	assert_success

	entry="node_modules/.aube/is-odd@3.0.1"
	expected="$(readlink "$entry")"
	[ -n "$expected" ]

	store_v1="$(aube store path)"
	index="$(find "$store_v1/index" -name 'is-odd@*' -print -quit)"
	[ -n "$index" ]
	# Remove a NON-first CAS shard so the cheap first-file probe inside
	# `load_index` still succeeds and only the verified walk catches it.
	victim="$(grep -o '"store_path":"[^"]*"' "$index" | sed -n '2p' | sed 's/.*":"//;s/"$//')"
	[ -n "$victim" ]
	rm -f "$victim"

	# Point the entry at a directory that exists but is not the target
	# this graph expects, and remove the expected target.
	decoy="$TEST_TEMP_DIR/decoy"
	mkdir -p "$decoy/node_modules"
	cp -r "$expected/node_modules/is-odd" "$decoy/node_modules/" 2>/dev/null || true
	rm -rf "${expected:?}"
	rm -f "$entry"
	ln -s "$decoy" "$entry"
	[ -e "$entry" ]

	run aube install
	assert_success
	assert_file_exists "$victim"
	assert_dir_exists "$expected"
	_assert_installed_is_odd
}

@test "install self-heals a stale index when the entry resolves elsewhere" {
	# Non-workspace form of the test above, and the one the shortcut is
	# actually enabled for. It used to fail with "failed to link
	# node_modules" on every retry: the shortcut classified the package
	# `AlreadyLinked` because its `.aube/<dep>` entry merely *resolved*,
	# so the linker was handed no index, found the entry `Stale` against
	# the target this graph expects, and fell back to the *unverified*
	# `store.load_index` — a stale index whose CAS shards are gone. Only
	# the fetch phase can re-download, so nothing recovered.
	#
	# The shortcut now compares the entry's target against the expected
	# virtual-store subdir, so this package misses it, takes the verified
	# index load, drops to `NeedsFetch`, and the store, the expected
	# target and the link are all restored.
	_setup_single_fixture

	run aube install
	assert_success

	entry="node_modules/.aube/is-odd@3.0.1"
	expected="$(readlink "$entry")"
	[ -n "$expected" ]

	store_v1="$(aube store path)"
	index="$(find "$store_v1/index" -name 'is-odd@*' -print -quit)"
	[ -n "$index" ]
	# Remove a NON-first CAS shard so the cheap first-file probe inside
	# `load_index` still succeeds and only the verified walk catches it.
	victim="$(grep -o '"store_path":"[^"]*"' "$index" | sed -n '2p' | sed 's/.*":"//;s/"$//')"
	[ -n "$victim" ]
	rm -f "$victim"

	# Point the entry at a directory that exists but is not the target
	# this graph expects, and remove the expected target.
	decoy="$TEST_TEMP_DIR/decoy"
	mkdir -p "$decoy/node_modules"
	cp -r "$expected/node_modules/is-odd" "$decoy/node_modules/" 2>/dev/null || true
	rm -rf "${expected:?}"
	rm -f "$entry"
	ln -s "$decoy" "$entry"
	[ -e "$entry" ]

	run aube install
	assert_success
	assert_file_exists "$victim"
	assert_dir_exists "$expected"
	_assert_installed_is_odd_root
}

@test "workspace install with a changed store-dir produces a correct tree" {
	# `has_explicit_store_dir_override` only inspects embedder and CLI
	# sources, so an `.npmrc` store-dir change leaves the shortcut
	# enabled. That is safe because the store is content addressed: the
	# already-materialized entries hold the same bytes the new store
	# would produce, and anything missing is fetched into the new store.
	_setup_ws_fixture

	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd

	other_store="$TEST_TEMP_DIR/other-store"
	mkdir -p "$other_store"
	printf 'store-dir=%s\n' "$other_store" >>.npmrc

	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd
	_assert_installed_is_odd
}

@test "lockfile content change busts the freshness fast path" {
	# The freshness check stats the root lockfile and skips the BLAKE3
	# re-hash when `(size, mtime)` is unchanged. Any content change moves
	# both, so it must still be caught — the fast path is an
	# optimization, not a weakening of the check.
	_setup_basic_fixture

	run aube install
	assert_success

	run aube run hello
	assert_success
	refute_output --partial "Auto-installing"

	printf '\n# edited\n' >>aube-lock.yaml

	run aube run hello
	assert_success
	assert_output --partial "Auto-installing"
	assert_output --partial "aube-lock.yaml has changed"
}

@test "touching the lockfile does not trigger a reinstall" {
	# mtime moved but content did not: the stat fast path misses, the
	# hash fallback confirms the lockfile is unchanged, and the refreshed
	# `(size, mtime)` snapshot is written back so the next check is
	# stat-only again.
	_setup_basic_fixture

	run aube install
	assert_success

	touch aube-lock.yaml

	run aube run hello
	assert_success
	refute_output --partial "Auto-installing"

	# Second run proves the refreshed snapshot persisted rather than
	# falling back to the hash forever.
	run aube run hello
	assert_success
	refute_output --partial "Auto-installing"
}
