#!/usr/bin/env bats

# Smoke tests for the `node_modules`-shape settings that don't expose a
# CLI flag and only surface through `.npmrc` / `pnpm-workspace.yaml`:
#
#   * enableModulesDir       — persistent equivalent of --lockfile-only
#   * virtualStoreDirMaxLength — cap on .aube/<name> directory names
#   * virtualStoreOnly       — populate .aube/ but skip top-level symlinks
#   * modulesCacheMaxAge     — sweep orphaned .aube/ entries after install
#   * symlink                — accept-and-warn; aube's isolated layout
#                              requires symlinks so symlink=false is a
#                              no-op but must not fail the install
#
# Each test runs against the offline Verdaccio fixture registry so
# behaviour is deterministic without network access.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "enableModulesDir=false writes the lockfile but skips node_modules" {
	_setup_basic_fixture

	echo "enableModulesDir=false" >>.npmrc

	run aube install
	assert_success
	# Lockfile updated (or fresh short-circuit hit), but the modules
	# tree is untouched — the whole point of the setting.
	assert_file_exists aube-lock.yaml
	assert [ ! -e node_modules ]
}

@test "enableModulesDir=false conflicts with lockfile=false" {
	_setup_basic_fixture

	cat >>.npmrc <<-EOF
		enableModulesDir=false
		lockfile=false
	EOF

	run aube install
	assert_failure
	assert_output --partial "enableModulesDir=false is incompatible with lockfile=false"
}

@test "virtualStoreDirMaxLength installs cleanly with a generous cap" {
	_setup_basic_fixture

	# Well above the default so nothing is truncated — proves the
	# setting parses and flows through the linker without crashing.
	echo "virtualStoreDirMaxLength=200" >>.npmrc

	run aube install
	assert_success
	assert_dir_exists node_modules/.aube
	assert_dir_exists node_modules/is-odd
}

@test "virtualStoreDirMaxLength=40 truncates .aube/<name> directories" {
	_setup_basic_fixture

	echo "virtualStoreDirMaxLength=40" >>.npmrc

	run aube install
	assert_success

	# Walk every .aube/<name> entry and verify the filename is <= 40
	# chars. The truncate-and-hash branch kicks in for anything that
	# would overflow, so the graph-hashed global-store paths (which
	# are longer) must not leak into .aube/.
	shopt -s nullglob
	for entry in node_modules/.aube/*/; do
		name="$(basename "$entry")"
		[ ${#name} -le 40 ] || {
			echo "entry $name is ${#name} chars (cap=40)"
			false
		}
	done
}

@test "virtualStoreOnly=true populates .aube but skips top-level symlinks" {
	_setup_basic_fixture

	echo "virtualStoreOnly=true" >>.npmrc

	run aube install
	assert_success

	# `.aube/` is populated.
	assert_dir_exists node_modules/.aube
	# But no top-level symlinks and no .bin.
	assert [ ! -e node_modules/is-odd ]
	assert [ ! -e node_modules/is-even ]
	assert [ ! -e node_modules/.bin ]
}

@test "virtualStoreOnly=false (default) writes the usual top-level symlinks" {
	_setup_basic_fixture

	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
	assert_dir_exists node_modules/is-even
}

@test "modulesCacheMaxAge=0 disables the orphan sweep" {
	_setup_basic_fixture

	run aube install
	assert_success
	# Seed a fake orphan entry that's older than any sensible
	# threshold — 90 days ago.
	orphan="node_modules/.aube/orphan-pkg@9.9.9"
	mkdir -p "$orphan/node_modules/orphan-pkg"
	touch -t 202401010000 "$orphan" "$orphan/node_modules" \
		"$orphan/node_modules/orphan-pkg"

	echo "modulesCacheMaxAge=0" >>.npmrc
	run aube install
	assert_success
	# max-age=0 turns the sweep off entirely; the orphan survives.
	assert_dir_exists "$orphan"
}

@test "modulesCacheMaxAge=1 removes orphaned .aube entries older than the threshold" {
	_setup_basic_fixture

	run aube install
	assert_success

	# Fabricate an orphan that's older than the 1-minute cap. Real
	# orphans show up when a dependency is removed from package.json;
	# for the test we can just plant one by hand and backdate its
	# mtime.
	orphan="node_modules/.aube/orphan-pkg@9.9.9"
	mkdir -p "$orphan/node_modules/orphan-pkg"
	touch -t 202401010000 "$orphan"

	echo "modulesCacheMaxAge=1" >>.npmrc
	run aube install
	assert_success

	# The orphan is gone — it isn't in the graph and its mtime is
	# way past the one-minute threshold.
	assert [ ! -e "$orphan" ]
	# Graph-referenced entries are always preserved regardless of age.
	assert_dir_exists node_modules/.aube
	assert_dir_exists node_modules/is-odd
}

@test "modulesCacheMaxAge keeps recent orphans" {
	_setup_basic_fixture

	run aube install
	assert_success

	# Plant a recent orphan — mtime is now, cap is 7 days (default).
	# Sweep must leave it alone.
	orphan="node_modules/.aube/fresh-orphan@0.0.1"
	mkdir -p "$orphan/node_modules/fresh-orphan"

	run aube install
	assert_success
	assert_dir_exists "$orphan"
}

@test "modulesCacheMaxAge preserves the .aube/node_modules hidden hoist tree" {
	# Defense for the cursor-bot review on PR #202: the sweep
	# iterates `.aube/*`, and the hidden hoist tree at
	# `.aube/node_modules/` (populated by `link_hidden_hoist`) is
	# not a `dep_path_to_filename` output, so it isn't in the
	# `in_use` set. Today the linker wipes-and-recreates the tree
	# on every install, which incidentally refreshes its mtime and
	# masks the issue — but if a future optimization preserves
	# unchanged hoist contents in place, the sweep would silently
	# nuke the directory and break Node's parent-walk resolution
	# for packages inside the virtual store. The explicit
	# "node_modules" skip in the sweep is the durable invariant;
	# this test pins it in place.
	_setup_basic_fixture

	run aube install
	assert_success
	assert_dir_exists node_modules/.aube/node_modules

	# Backdate the hidden hoist directory's mtime so it would be
	# eligible for removal under any non-zero threshold *if* the
	# linker preserved its mtime instead of refreshing it.
	touch -t 202401010000 node_modules/.aube/node_modules

	echo "modulesCacheMaxAge=1" >>.npmrc
	run aube install
	assert_success
	# The hidden hoist tree must survive the sweep regardless of
	# whether the linker happened to refresh its mtime.
	assert_dir_exists node_modules/.aube/node_modules
}

@test "modulesCacheMaxAge unlinks orphan symlinks without touching the target" {
	# Defense for the cursor-bot review on PR #202: in
	# global-virtual-store mode `.aube/<dep>` entries are symlinks
	# into the shared `~/.cache/aube/virtual-store/`. On older
	# Linux kernels `remove_dir_all` could follow the symlink and
	# recursively destroy the shared target, breaking other
	# projects. The sweep must only unlink the local symlink.
	_setup_basic_fixture

	run aube install
	assert_success

	# Sentinel directory that stands in for a shared virtual-store
	# target. Place it outside the project so an over-eager
	# `remove_dir_all` would still wipe it but a `remove_file` on
	# the symlink leaves it intact.
	sentinel="$BATS_TEST_TMPDIR/sentinel-target"
	mkdir -p "$sentinel"
	echo "do not delete" >"$sentinel/canary.txt"

	# Plant an orphan symlink in .aube pointing at the sentinel
	# and backdate it so the sweep treats it as a removal candidate.
	orphan="node_modules/.aube/orphan-symlink@9.9.9"
	ln -s "$sentinel" "$orphan"
	touch -h -t 202401010000 "$orphan" 2>/dev/null || touch -t 202401010000 "$orphan"

	echo "modulesCacheMaxAge=1" >>.npmrc
	run aube install
	assert_success

	# Local symlink unlinked, but the sentinel target survives.
	assert [ ! -e "$orphan" ]
	assert_dir_exists "$sentinel"
	assert_file_exists "$sentinel/canary.txt"
}

@test "modulesCacheMaxAge --prod doesn't sweep dev deps from .aube" {
	# Regression for the cursor-bot review on PR #202: building the
	# in-use set from `graph_for_link` instead of the unfiltered
	# `graph` made dev deps eligible for the sweep on `--prod`
	# installs, forcing a full re-fetch on the next non-prod
	# install.
	cat >package.json <<-EOF
		{
		  "name": "modules-cache-prod-fixture",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^3.0.1" },
		  "devDependencies": { "kind-of": "^6.0.3" }
		}
	EOF

	# Establish the .aube state with both prod + dev deps materialized.
	run aube install
	assert_success
	assert_dir_exists node_modules/.aube
	# The dev dep's .aube entry should exist before we re-install
	# under --prod.
	dev_aube=$(find node_modules/.aube -maxdepth 1 -name 'kind-of@*' -print -quit)
	[ -n "$dev_aube" ] || {
		echo "expected node_modules/.aube/kind-of@... before --prod install"
		false
	}
	# Backdate the dev dep's mtime so it would be a sweep candidate
	# *if* the in-use set were built from the filtered graph.
	touch -t 202401010000 "$dev_aube"

	# `--prod` install with a 1-minute cap. The sweep must look at
	# the unfiltered graph (which still includes the dev dep) and
	# leave the .aube entry alone.
	echo "modulesCacheMaxAge=1" >>.npmrc
	run aube install --prod
	assert_success
	assert_dir_exists "$dev_aube"
}

# `symlink` exists for pnpm parity. aube's isolated layout is the
# symlink graph under node_modules/.aube/, so `symlink=false` cannot
# switch to a hard-copy layout — we accept the value (so a `.npmrc`
# ported from pnpm keeps loading) and print a single warning.

@test "symlink=false emits a warning and still installs with symlinks" {
	_setup_basic_fixture

	echo "symlink=false" >>.npmrc

	run aube install
	assert_success
	assert_output --partial "aube's isolated layout requires symlinks"
	assert_output --partial 'symlink=false is accepted but has no effect'
	# Top-level entry is still a symlink — the setting is a no-op.
	assert [ -L node_modules/is-odd ]
}

@test "symlink=true (default) is silent" {
	_setup_basic_fixture

	run aube install
	assert_success
	refute_output --partial "aube's isolated layout requires symlinks"
	assert [ -L node_modules/is-odd ]
}
