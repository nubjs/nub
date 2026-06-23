#!/usr/bin/env bats

# Toggling `enableGlobalVirtualStore` across installs must wipe
# `node_modules/` before the linker runs. The linker can't reconcile
# the switch in place — stale symlinks survive the non-gvs pass, and
# populated directories block the gvs pass — so the install driver
# detects the mismatch up front, warns, and rebuilds from scratch.
# Matches pnpm's behavior modulo the prompt.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "disabling gvs via .npmrc wipes the previous gvs tree on next install" {
	_setup_basic_fixture

	run aube install
	assert_success
	# Initial state: gvs on, `.aube/<pkg>` is a symlink into the
	# shared store.
	[ -L node_modules/.aube/is-odd@3.0.1 ]

	cat >.npmrc <<'RC'
enableGlobalVirtualStore=false
RC

	run aube install
	assert_success
	assert_output --partial "global virtual store enabled → disabled"
	assert_output --partial "removing"
	# After the wipe + re-link: `.aube/<pkg>` is a real directory.
	[ -d node_modules/.aube/is-odd@3.0.1 ]
	[ ! -L node_modules/.aube/is-odd@3.0.1 ]
}

@test "re-enabling gvs via .npmrc wipes the previous per-project tree on next install" {
	_setup_basic_fixture

	cat >.npmrc <<'RC'
enableGlobalVirtualStore=false
RC
	run aube install
	assert_success
	[ -d node_modules/.aube/is-odd@3.0.1 ]
	[ ! -L node_modules/.aube/is-odd@3.0.1 ]

	rm .npmrc

	run aube install
	assert_success
	assert_output --partial "global virtual store disabled → enabled"
	assert_output --partial "removing"
	[ -L node_modules/.aube/is-odd@3.0.1 ]
}

@test "gvs setting unchanged across reinstalls does not emit the transition warning" {
	_setup_basic_fixture

	run aube install
	assert_success

	run aube install
	assert_success
	refute_output --partial "global virtual store"
}
