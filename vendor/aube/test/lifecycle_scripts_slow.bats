#!/usr/bin/env bats
#
# Network-dependent ports of pnpm/test/install/lifecycleScripts.ts.
# These exercise paths that hit real upstream services (github.com
# clone + a git-dep `prepare` chain that pulls TypeScript from the
# real npm registry), which the offline Verdaccio fixture can't host.
#
# Gated behind AUBE_NETWORK_TESTS=1 so the default `mise run test:bats`
# stays offline. Same convention as test/pnpm_install_misc_slow.bats
# and test/pnpm_update_slow.bats.

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

@test "aube add: git dependency with prepare script under dangerouslyAllowAllBuilds" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:336
	# ('git dependencies with preparation scripts should be installed
	# when dangerouslyAllowAllBuilds is true').
	#
	# Fixture: pnpm/test-git-fetch.git pinned to the same SHA pnpm uses.
	# The repo's `prepare` script runs `tsc src/index.ts --outDir dist`,
	# which requires its devDependency `typescript` to be installed
	# during the git-dep bootstrap — exercising aube's full git-dep
	# lifecycle: clone → sub-install devDeps → run prepare → pack →
	# materialize. The prepare/preinstall/install/postinstall chain
	# is gated by `dangerouslyAllowAllBuilds: true`.
	_require_network

	cat >package.json <<'JSON'
{
  "name": "git-prepare-dangerously-allow-all-builds",
  "version": "1.0.0"
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
dangerouslyAllowAllBuilds: true
YAML
	# The git-dep prepare bootstrap runs a nested install for the
	# fixture's devDependencies (`typescript@^4.2.4`). The default
	# bats registry (Verdaccio at localhost:4873) is offline-only and
	# doesn't host typescript, so override the registry for the
	# duration of this test to hit the real npmjs.org.
	echo "registry=https://registry.npmjs.org/" >.npmrc
	run aube add 'https://github.com/pnpm/test-git-fetch.git#8b333f12d5357f4f25a654c305c826294cb073bf'
	assert_success
	# `prepare` ran tsc and produced dist/index.js — that's the
	# regression guard. If prepare was silently skipped (the bug
	# `dangerouslyAllowAllBuilds` is meant to fix), the file is absent.
	assert_file_exists node_modules/test-git-fetch/dist/index.js
}
