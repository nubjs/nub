#!/usr/bin/env bats
#
# Network-dependent ports of pnpm/test/install/misc.ts. These exercise
# paths that hit real upstream services (github.com codeload), which the
# offline Verdaccio fixture can't host.
#
# Gated behind AUBE_NETWORK_TESTS=1 so the default `mise run test:bats`
# stays offline. CI opts in by setting the env var explicitly. Same
# convention as test/pnpm_update_slow.bats.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_require_network() {
	if [ "${AUBE_NETWORK_TESTS:-}" != "1" ]; then
		skip "set AUBE_NETWORK_TESTS=1 to run network tests"
	fi
}

@test "aube install: git URL with hash containing slash" {
	# Ported from pnpm/test/install/misc.ts:567
	# ('install success even though the url's hash contains slash').
	# Covers https://github.com/pnpm/pnpm/issues/7697. Uses pnpm's own
	# upstream fixture (github.com/pnpm-e2e/simple-pkg, branch
	# `branch/with-slash`) — a 1-file repo whose `branch/with-slash`
	# branch is pinned to a stable SHA. Regression guard: aube's
	# fragment parser at aube-lockfile/src/lib.rs:645 routes the
	# slash-bearing fragment into the `""` fallback branch and stores
	# it as the committish, and the git resolver at
	# aube-resolver/src/local_source.rs:242 walks the resulting
	# GitSource.committish into `git ls-remote` to pin the SHA.
	_require_network

	cat >package.json <<'JSON'
{
  "name": "aube-git-slash-fragment",
  "version": "0.0.0",
  "dependencies": {
    "simple-pkg": "git+https://github.com/pnpm-e2e/simple-pkg.git#branch/with-slash"
  }
}
JSON

	run aube install
	assert_success
	# Lockfile records the resolved committish — proves the slash-
	# bearing fragment survived parse_git_spec and made it through
	# `git ls-remote`. The SHA below is the head of `branch/with-slash`
	# at fixture-creation time and is stable (the branch is not
	# advanced upstream).
	assert_file_exists aube-lock.yaml
	run grep -F '2fce895ee534a38989bb67fdb8684f520827f614' aube-lock.yaml
	assert_success
	# And the package landed in node_modules.
	assert_file_exists node_modules/simple-pkg/package.json
}
