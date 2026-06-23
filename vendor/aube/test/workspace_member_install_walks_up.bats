#!/usr/bin/env bats
#
# Regression: `aube install` run from inside a workspace member must
# resolve up to the workspace root (pnpm parity) instead of treating the
# member as a standalone project. The user-visible bug is that a member
# install runs a fresh resolve/fetch/link cycle and writes a private
# lockfile, virtual store, and install state next to the member's own
# package.json — duplicating work the prior workspace-root install
# already did, and re-downloading anything not already in the global
# cache.
#
# Root cause: install/mod.rs's `find_project_root` walks up to the
# *nearest* package.json, which inside a workspace is the member's own.
# A workspace-aware walk should resolve to the workspace root when the
# member's ancestor is a workspace declaration.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_minimal_workspace() {
	cat >package.json <<-'JSON'
		{
		  "name": "ws-root",
		  "version": "0.0.0",
		  "private": true
		}
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - packages/*
	YAML
	mkdir -p packages/a
	cat >packages/a/package.json <<-'JSON'
		{
		  "name": "@ws/a",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1"
		  }
		}
	JSON
}

@test "aube install from a workspace member resolves up to the workspace root" {
	_setup_minimal_workspace

	# Prime the workspace at the root. After this, the workspace root
	# owns the lockfile, the virtual store, and the install state.
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
	assert_dir_exists node_modules/.aube
	assert_dir_exists node_modules/.aube-state
	assert_file_exists node_modules/.aube-state/state.json
	assert_link_exists packages/a/node_modules/is-odd

	# Snapshot the root install state. A correct member install must
	# resolve back up to this same root and short-circuit on the warm
	# fast path — the root state must be byte-identical afterward.
	local root_state_before
	root_state_before="$(cat node_modules/.aube-state/state.json)"

	cd packages/a
	run aube install
	assert_success
	cd ../..

	# No standalone artifacts inside the workspace member. Each
	# assertion below proves a distinct symptom of the bug:
	#   - own lockfile  -> member ran its own resolve
	#   - own .aube     -> member ran its own linker pass
	#   - own .aube-state -> member ran the full install pipeline
	# Any single failure means the member install ignored the
	# workspace root.
	run test -e packages/a/aube-lock.yaml
	assert_failure
	run test -d packages/a/node_modules/.aube
	assert_failure
	run test -e packages/a/node_modules/.aube-state
	assert_failure

	# The root install state is byte-identical — the only correct
	# warm path is "do nothing", and "do nothing" can't write any
	# state directory. `assert_equal` (not `[`) so a regression prints
	# both snapshots and we can diff what changed.
	assert_equal "$(cat node_modules/.aube-state/state.json)" "$root_state_before"
}
