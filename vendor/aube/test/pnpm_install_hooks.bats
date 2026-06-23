#!/usr/bin/env bats
#
# Ported from pnpm/test/install/hooks.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Coverage focus: .pnpmfile.cjs hook behavior — readPackage (sync/async),
# afterAllResolved (sync/async), preResolution, the `--pnpmfile` /
# `--global-pnpmfile` CLI flags, the ndjson `pnpm:hook` log surface, and
# pnpmfile load-time error paths. The Tier 1 `@pnpm.e2e/*` fixtures are
# mirrored under test/registry/storage/, so newer ports use them
# directly; older ports lean on in-tree generic fixtures (is-odd,
# is-even, is-positive, is-negative) where the behavior is
# package-agnostic.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "pnpmfile: async readPackage hook mutates transitive dependencies" {
	# Ported from pnpm/test/install/hooks.ts:43 ('readPackage async hook').
	# Substitution: pnpm's @pnpm.e2e/pkg-with-1-dep + addDistTag → is-even
	# (pulls in is-odd transitively). The hook strips is-odd's deps.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-readpackage-async",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    async readPackage (pkg) {
      if (pkg.name === 'is-odd') {
        pkg.dependencies = {}
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	# The hook cleared is-odd's dependencies; the snapshot entry should
	# be empty rather than pulling in is-number / kind-of.
	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | grep '^  is-odd@'"
	assert_output --partial 'is-odd@0.1.2: {}'
	# Belt-and-braces: confirm the transitive chain is-odd → is-number
	# → kind-of was actually short-circuited, not just hidden in the
	# snapshot. A regression that empties the snapshot entry while still
	# resolving the deps would otherwise sneak through.
	run grep 'is-number' aube-lock.yaml
	assert_failure
	run grep 'kind-of' aube-lock.yaml
	assert_failure
}

@test "pnpmfile: async afterAllResolved hook runs and is awaited" {
	# Ported from pnpm/test/install/hooks.ts:498 ('pnpmfile: run async
	# afterAllResolved hook'). pnpm asserts on ndjson reporter logs;
	# aube's reporter pipeline doesn't surface context.log to stdout the
	# same way, so we assert the install completes (i.e. the async hook
	# is awaited and its return value reused) and that the lockfile got
	# written.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-afterallresolved-async",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    async afterAllResolved (lockfile, context) {
      // No mutation; just exercise the async path. Return a promise
      // that resolves on the next tick so we know aube actually
      // awaited it rather than dropping it on the floor.
      await new Promise((resolve) => setImmediate(resolve))
      return lockfile
    }
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
	assert_file_exists node_modules/is-odd/index.js
}

@test "pnpmfile: syntax error in .pnpmfile.cjs fails the install" {
	# Ported from pnpm/test/install/hooks.ts:292 ('prints meaningful error
	# when there is syntax error in .pnpmfile.cjs').
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-syntax-error",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	# `/boom` parses as the start of a regex literal with no closing /.
	echo '/boom' >.pnpmfile.cjs

	run aube install
	assert_failure
	# The exact wording may differ from pnpm; just assert aube surfaces
	# *some* signal that the pnpmfile is the culprit.
	assert_output --partial 'pnpmfile'
}

@test "pnpmfile: require() of a missing module fails the install" {
	# Ported from pnpm/test/install/hooks.ts:303 ('fails when .pnpmfile.cjs
	# requires a non-existed module').
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-require-missing",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON
	echo 'module.exports = require("./this-does-not-exist")' >.pnpmfile.cjs

	run aube install
	assert_failure
	assert_output --partial 'pnpmfile'
}

@test "pnpmfile: readPackage hook can set optionalDependencies / peerDependencies / devDependencies on a transitive" {
	# Ported from pnpm/test/install/hooks.ts:528 ('readPackage hook
	# normalizes the package manifest'). Substitution: pnpm's
	# @pnpm.e2e/dep-of-pkg-with-1-dep → is-odd; is-positive / is-negative
	# → is-number (already in test/registry/storage/). Verifies the
	# resolver doesn't choke when the hook sets optional/peer/dev fields
	# on a transitive that doesn't originally declare them.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-normalize",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'is-odd') {
        pkg.optionalDependencies = pkg.optionalDependencies || {}
        pkg.optionalDependencies['is-number'] = '*'
        pkg.peerDependencies = pkg.peerDependencies || {}
        pkg.peerDependencies['is-number'] = '*'
        pkg.devDependencies = pkg.devDependencies || {}
        pkg.devDependencies['is-number'] = '*'
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists node_modules/is-even/index.js
	# is-odd is transitive (via is-even), so it lands in the virtual
	# store rather than at node_modules root. The peer-context suffix
	# (`_is-number@3.0.0`) is what tells us aube actually applied the
	# hook's peerDependencies edit before resolving.
	assert_dir_exists node_modules/.aube/is-odd@0.1.2_is-number@3.0.0/node_modules/is-odd
}

@test "pnpmfile: readPackage hook fires during aube update" {
	# Ported from pnpm/test/install/hooks.ts:263 ('readPackage hook during
	# update'). pnpm relies on addDistTag to publish a newer version
	# that `update` then resolves through the hook; aube's offline
	# registry fixtures don't have an addDistTag analogue, so we instead
	# run `update` from scratch with the hook already wired in. The
	# hook's mutation must land in the lockfile that update writes —
	# which it can only do if update's resolver attaches the readPackage
	# host (the gap this test was added to close).
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-readpackage-during-update",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'is-odd') {
        pkg.dependencies = {}
      }
      return pkg
    }
  }
}
EOF

	run aube update
	assert_success
	# Guard against the false-pass where `aube update` leaves no
	# lockfile and the negative greps below trip on grep's
	# exit-2-for-file-not-found rather than on the hook actually
	# stripping the chain.
	assert_file_exists aube-lock.yaml
	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | grep '^  is-odd@'"
	assert_output --partial 'is-odd@0.1.2: {}'
	# The transitive chain (is-odd → is-number → kind-of) is gone
	# because the hook stripped is-odd's deps before the resolver
	# walked them. Without the readPackage host wired into update,
	# is-number and kind-of would still appear.
	run grep 'is-number' aube-lock.yaml
	assert_failure
	run grep 'kind-of' aube-lock.yaml
	assert_failure
}

@test "pnpmfile: --ignore-pnpmfile during aube update" {
	# Ported from pnpm/test/install/hooks.ts:338 ('ignore .pnpmfile.cjs
	# during update when --ignore-pnpmfile is used').
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-ignore-pnpmfile-update",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	# Step 1: install with NO pnpmfile — full transitive chain in the lockfile.
	run aube install
	assert_success

	# Step 2: drop in a hook that would strip is-odd's deps.
	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'is-odd') {
        pkg.dependencies = {}
      }
      return pkg
    }
  }
}
EOF

	# With --ignore-pnpmfile the hook should NOT fire, so is-odd's
	# transitive deps stay intact in the lockfile after update.
	run aube update --ignore-pnpmfile
	assert_success
	run grep 'is-number' aube-lock.yaml
	assert_success
	run grep 'kind-of' aube-lock.yaml
	assert_success
}

@test "pnpmfile: preResolution hook fires before resolve" {
	# Ported from pnpm/test/install/hooks.ts:624 ('preResolution hook').
	# The pnpm test asserts the hook receives a resolution context with
	# currentLockfile / wantedLockfile / registries / lockfileDir /
	# storeDir. We assert on a subset that's stable across aube's
	# context shape — the existence of the file proves the hook fired,
	# and the lockfileDir + registries fields prove we're passing the
	# pnpm-shaped ctx, not just an empty object.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-preresolution",
  "version": "0.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
const fs = require('fs')
module.exports = {
  hooks: {
    preResolution (ctx) {
      fs.writeFileSync('preresolution-fired.json', JSON.stringify(ctx))
    }
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists preresolution-fired.json
	run cat preresolution-fired.json
	assert_output --partial '"lockfileDir"'
	assert_output --partial '"registries"'
	assert_output --partial '"existsCurrentLockfile":false'
}

@test "pnpmfile: readPackage that returns undefined fails the install" {
	# Ported from pnpm/test/install/hooks.ts:68 ('readPackage hook makes
	# installation fail if it does not return the modified package
	# manifests').
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-undef-return",
  "version": "0.0.0",
  "dependencies": { "is-even": "1.0.0" }
}
JSON

	# A "bad" hook that forgets to return the manifest. Scoped to the
	# dependency on purpose: aube now also fires readPackage on the root
	# importer manifest (pnpm parity), so an unconditional bad hook would
	# fail against the root package first. Targeting is-even keeps the
	# assertion focused on the dependency-level failure this test ports.
	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'is-even') return
      return pkg
    }
  }
}
EOF

	run aube install
	assert_failure
	assert_output --partial 'package manifest object'
	assert_output --partial 'is-even@1.0.0'
}

@test "pnpmfile: readPackage hook can mutate the root project's dependencies" {
	# Ported from pnpm/test/install/hooks.ts:551 ('readPackage hook
	# overrides project package'). aube now fires readPackage on importer
	# manifests too (not just resolved registry packages), matching pnpm.
	cat >package.json <<'JSON'
{
  "name": "test-read-package-hook",
  "version": "0.0.0"
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'test-read-package-hook') {
        pkg.dependencies = { 'is-odd': '3.0.1' }
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	assert_file_exists node_modules/is-odd/index.js
	# package.json on disk should NOT have been written — the hook only
	# mutates the in-memory manifest.
	run cat package.json
	refute_output --partial '"dependencies"'
}

# Helper: assert that a specific @pnpm.e2e/dep-of-pkg-with-1-dep version
# was materialized in the project. The pnpm tests use `project.storeHas`
# which checks pnpm's content-addressed store — aube's CAS layout is
# different, but the public-facing equivalent is "the version landed in
# node_modules", which we read out of `node_modules/<pkg>/package.json`.
_assert_dep_version() {
	local expected="$1"
	local pkg_json="node_modules/.aube/@pnpm.e2e+pkg-with-1-dep@100.0.0/node_modules/@pnpm.e2e/dep-of-pkg-with-1-dep/package.json"
	# `node_modules/.aube/<dep_path>/node_modules/<name>/package.json` is the
	# canonical materialization site; isolated-mode symlinks point into it.
	run jq -r .version "$pkg_json"
	assert_success
	assert_output "$expected"
}

@test "pnpmfile: --pnpmfile loads readPackage from a custom location" {
	# Ported from pnpm/test/install/hooks.ts:85
	# ('readPackage hook from custom location').
	# Substitution: pnpm uses `pnpm install <pkg> --pnpmfile pnpm.js`.
	# aube's `--pnpmfile` flag is parsed identically. Without the hook,
	# `^100.0.0` would resolve to 100.1.0 (the registry's `latest` for
	# @pnpm.e2e/dep-of-pkg-with-1-dep); the hook rewrites the dep spec
	# to a hard-pin `100.0.0` so the resolver picks the older version.
	cat >package.json <<'JSON'
{
  "name": "test-pnpmfile-flag",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.0.0" }
}
JSON

	# Note the non-default filename (`pnpm.js`) — the whole point of
	# `--pnpmfile` is to load a hook file that wouldn't be picked up by
	# the default `.pnpmfile.mjs` / `.pnpmfile.cjs` discovery.
	cat >pnpm.js <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.0.0'
      }
      return pkg
    }
  }
}
EOF

	run aube install --pnpmfile pnpm.js
	assert_success
	_assert_dep_version 100.0.0
}

@test "pnpmfile: --global-pnpmfile loads readPackage from outside the project" {
	# Ported from pnpm/test/install/hooks.ts:110
	# ('readPackage hook from global pnpmfile').
	# pnpm writes the global pnpmfile to `..` and passes its absolute
	# path. We do the same here — a sibling directory under
	# $TEST_TEMP_DIR holds the global pnpmfile, and the project itself
	# lives in a subdirectory.
	mkdir -p project
	cd project
	cat >package.json <<'JSON'
{
  "name": "test-global-pnpmfile",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.0.0" }
}
JSON

	# Global pnpmfile lives in the parent dir — pnpm uses `path.resolve('..',
	# '.pnpmfile.cjs')` for this exact pattern.
	cat >../global-hooks.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.0.0'
      }
      return pkg
    }
  }
}
EOF

	run aube install --global-pnpmfile "$(cd .. && pwd)/global-hooks.cjs"
	assert_success
	_assert_dep_version 100.0.0
}

@test "pnpmfile: global + local hooks compose, local overrides global" {
	# Ported from pnpm/test/install/hooks.ts:135
	# ('readPackage hook from global pnpmfile and local pnpmfile').
	# Substitution: pnpm uses `is-positive@1.0.0`/`is-positive@3.0.0` —
	# both versions are now mirrored under test/registry/storage/, but
	# this port predates that fixture and uses is-number@3.0.0/7.0.0
	# instead, which fits the same two-distinct-versions shape. Verifies
	# pnpm's composition order: global runs first, local runs second, so
	# a field both hooks touch ends up at the local value while a field
	# only the global hook sets survives.
	mkdir -p project
	cd project
	cat >package.json <<'JSON'
{
  "name": "test-pnpmfile-compose",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.0.0" }
}
JSON

	# Global pins both `dep-of-pkg-with-1-dep` (100.0.0, no override
	# from local) AND `is-number` (7.0.0, will be overridden).
	cat >../global-hooks.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.0.0'
        pkg.dependencies['is-number'] = '7.0.0'
      }
      return pkg
    }
  }
}
EOF

	# Local only touches `is-number`, downgrading to 3.0.0.
	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['is-number'] = '3.0.0'
      }
      return pkg
    }
  }
}
EOF

	run aube install --global-pnpmfile "$(cd .. && pwd)/global-hooks.cjs"
	assert_success
	_assert_dep_version 100.0.0
	# is-number 3.0.0 must win — proves local ran after global.
	run jq -r .version "node_modules/.aube/@pnpm.e2e+pkg-with-1-dep@100.0.0/node_modules/is-number/package.json"
	assert_success
	assert_output 3.0.0
}

@test "pnpmfile: async global + async local readPackage compose" {
	# Ported from pnpm/test/install/hooks.ts:176
	# ('readPackage async hook from global pnpmfile and local pnpmfile').
	# Same shape as the previous test, except both hooks are declared
	# `async`. Confirms aube awaits each link in the composition chain
	# rather than dropping the promise on the floor.
	mkdir -p project
	cd project
	cat >package.json <<'JSON'
{
  "name": "test-pnpmfile-compose-async",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.0.0" }
}
JSON

	cat >../global-hooks.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    async readPackage (pkg) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.0.0'
        pkg.dependencies['is-number'] = '7.0.0'
      }
      return pkg
    }
  }
}
EOF

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    async readPackage (pkg) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['is-number'] = '3.0.0'
      }
      return pkg
    }
  }
}
EOF

	run aube install --global-pnpmfile "$(cd .. && pwd)/global-hooks.cjs"
	assert_success
	_assert_dep_version 100.0.0
	run jq -r .version "node_modules/.aube/@pnpm.e2e+pkg-with-1-dep@100.0.0/node_modules/is-number/package.json"
	assert_success
	assert_output 3.0.0
}

@test "pnpmfile: readPackage ctx.log surfaces as pnpm:hook ndjson on stdout" {
	# Ported from pnpm/test/install/hooks.ts:366 ('pnpmfile: pass log
	# function to readPackage hook'). pnpm uses `addDistTag` to set
	# latest=100.1.0 so 100.1.0 would be installed without the hook;
	# aube's storage already serves dep-of-pkg-with-1-dep@100.0.0/100.1.0
	# under the `^100.0.0` constraint that pkg-with-1-dep@100.1.0 declares,
	# so the hook's pin to 100.0.0 is a downgrade either way and we don't
	# need to mutate storage dist-tags.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-readpackage-ndjson-log",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.1.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg, ctx) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.0.0'
        ctx.log('@pnpm.e2e/dep-of-pkg-with-1-dep pinned to 100.0.0')
      }
      return pkg
    }
  }
}
EOF

	run aube install --reporter=ndjson
	assert_success
	install_stdout="$output"
	# Hook actually fired: the pin reroutes resolution from 100.1.0 to
	# 100.0.0. Without context.log being honored on stdout, this stays
	# the only ground truth, so guard it explicitly. `run` clobbers
	# `$output` (with grep's empty stdout under -q), but we already
	# stashed install_stdout above so the jq parse below is unaffected.
	run grep -q '@pnpm.e2e/dep-of-pkg-with-1-dep@100.0.0' aube-lock.yaml
	assert_success

	# pnpm:hook ndjson record on stdout. Same shape as pnpm:
	# {name,prefix,from,hook,message}. Use jq to parse line-by-line and
	# pick the record whose `name == "pnpm:hook"`.
	hook_log=$(printf '%s\n' "$install_stdout" | jq -c 'select(.name == "pnpm:hook")' 2>/dev/null | head -n 1)
	[ -n "$hook_log" ] || {
		echo "no pnpm:hook record in stdout"
		echo "stdout was: $install_stdout"
		false
	}
	[ "$(printf '%s' "$hook_log" | jq -r '.hook')" = readPackage ]
	[ "$(printf '%s' "$hook_log" | jq -r '.message')" = '@pnpm.e2e/dep-of-pkg-with-1-dep pinned to 100.0.0' ]
	# `prefix` and `from` are pnpm-shaped — non-empty paths.
	[ -n "$(printf '%s' "$hook_log" | jq -r '.prefix')" ]
	[ -n "$(printf '%s' "$hook_log" | jq -r '.from')" ]
}

@test "pnpmfile: afterAllResolved ctx.log surfaces as pnpm:hook ndjson on stdout" {
	# Ported from pnpm/test/install/hooks.ts:468 ('pnpmfile: run
	# afterAllResolved hook'). Uses pnpm's @pnpm.e2e/pkg-with-1-dep
	# fixture verbatim to match the pnpm test exactly.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-afterallresolved-ndjson-log",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.1.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    afterAllResolved (lockfile, ctx) {
      ctx.log('All resolved')
      return lockfile
    }
  }
}
EOF

	run aube install --reporter=ndjson
	assert_success
	hook_log=$(printf '%s\n' "$output" | jq -c 'select(.name == "pnpm:hook" and .hook == "afterAllResolved")' 2>/dev/null | head -n 1)
	[ -n "$hook_log" ] || {
		echo "no afterAllResolved pnpm:hook record in stdout"
		echo "stdout was: $output"
		false
	}
	[ "$(printf '%s' "$hook_log" | jq -r '.message')" = 'All resolved' ]
	[ -n "$(printf '%s' "$hook_log" | jq -r '.prefix')" ]
	[ -n "$(printf '%s' "$hook_log" | jq -r '.from')" ]
}

@test "pnpmfile: sync readPackage hook pins a transitive dep" {
	# Ported from pnpm/test/install/hooks.ts:18 ('readPackage hook').
	# Sync sibling of the async port at the top of the file. Now that
	# @pnpm.e2e/pkg-with-1-dep is mirrored under test/registry/storage/,
	# the port matches pnpm verbatim — only the addDistTag call is
	# dropped. With no hook, ^100.0.0 (declared by pkg-with-1-dep@100.0.0)
	# resolves to dep-of-pkg-with-1-dep@100.1.0 — the highest in range
	# regardless of the registry's `latest` dist-tag — so add_dist_tag
	# would be redundant here. The hook downgrades the pin to 100.0.0.
	# pkg-with-1-dep is pinned to 100.0.0 so the _assert_dep_version
	# helper's hardcoded virtual-store path resolves.
	cat >package.json <<'JSON'
{
  "name": "pnpm-hooks-readpackage-sync",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.0.0" }
}
JSON

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.0.0'
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	_assert_dep_version 100.0.0
}

@test "pnpmfile: workspace-root pnpmfile applies during install and sub-project add" {
	# Ported from pnpm/test/install/hooks.ts:217 ('readPackage hook from
	# pnpmfile at root of workspace'). The root .pnpmfile.cjs adds
	# @pnpm.e2e/dep-of-pkg-with-1-dep@100.1.0 to *every* resolved
	# package's dependencies map. We exercise two paths:
	#  (1) `aube install` from the workspace root — the resolver picks
	#      the root pnpmfile up via the workspace-root cwd it walks to
	#      from any subdirectory.
	#  (2) `aube add is-negative@1.0.0` from project-1 — `aube add`
	#      writes into project-1's package.json but transitions to
	#      install::run, which also walks up to the workspace root, so
	#      the same root pnpmfile fires on the new is-negative resolve.
	# The shared workspace lockfile at the root must reflect the hook's
	# additions on both is-positive (added in step 1) and is-negative
	# (added in step 2).
	mkdir -p project-1
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": { "is-positive": "1.0.0" }
}
JSON

	# `aube install` walks up to a workspace root that has its own
	# package.json — preparePackages writes one implicitly in pnpm.
	cat >package.json <<'JSON'
{ "name": "workspace-root", "version": "0.0.0" }
JSON

	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
YAML

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      pkg.dependencies = pkg.dependencies || {}
      pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.1.0'
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success

	# Subshell `cd` keeps `run` semantics intact (status/output captured)
	# while letting us return to the workspace root for the lockfile
	# assertions without a `cd ..` shellcheck flags as fragile.
	run bash -c 'cd project-1 && aube add is-negative@1.0.0'
	assert_success

	assert_link_exists project-1/node_modules/is-positive
	assert_link_exists project-1/node_modules/is-negative

	# Shared workspace lockfile at the root. Both is-positive@1.0.0 and
	# is-negative@1.0.0 snapshots must include the hook-injected
	# transitive — neither package declares any deps natively, so the
	# only way these entries land is via the readPackage rewrite.
	assert_file_exists aube-lock.yaml
	# Outer awk scopes to the snapshots: section (so we don't match the
	# importer-side dep declarations above); inner awk extracts the
	# specific snapshot block, walking forward until the next sibling key
	# at 2-space indent — resilient to future fields (`resolution:`, etc.)
	# being inserted before `dependencies:` inside the block.
	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | awk '/^  is-positive@1.0.0:\$/{flag=1; next} /^  [^ ]/{flag=0} flag'"
	assert_output --partial '@pnpm.e2e/dep-of-pkg-with-1-dep'
	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | awk '/^  is-negative@1.0.0:\$/{flag=1; next} /^  [^ ]/{flag=0} flag'"
	assert_output --partial '@pnpm.e2e/dep-of-pkg-with-1-dep'
}

@test "pnpmfile: global + local readPackage ctx.log emits one pnpm:hook record per pnpmfile" {
	# Ported from pnpm/test/install/hooks.ts:404 ('pnpmfile: pass log
	# function to readPackage hook of global and local pnpmfile'). Same
	# global-then-local composition shape as the existing compose tests
	# (462, 523), but each hook invokes ctx.log so we can verify the
	# ndjson reporter emits two pnpm:hook records — one tagged with the
	# global pnpmfile's `from` path, one with the local's. Both share
	# the same project `prefix`. is-positive 3.0.0 (global) is overridden
	# to 1.0.0 by the local hook, mirroring pnpm's assertion that local
	# wins on a field both touch.
	mkdir -p project
	cd project
	cat >package.json <<'JSON'
{
  "name": "test-pnpmfile-compose-log",
  "version": "0.0.0",
  "dependencies": { "@pnpm.e2e/pkg-with-1-dep": "100.1.0" }
}
JSON

	cat >../global-hooks.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg, context) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['@pnpm.e2e/dep-of-pkg-with-1-dep'] = '100.0.0'
        pkg.dependencies['is-positive'] = '3.0.0'
        context.log('is-positive pinned to 3.0.0')
      }
      return pkg
    }
  }
}
EOF

	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg, context) {
      if (pkg.name === '@pnpm.e2e/pkg-with-1-dep') {
        pkg.dependencies['is-positive'] = '1.0.0'
        context.log('is-positive pinned to 1.0.0')
      }
      return pkg
    }
  }
}
EOF

	run aube install --global-pnpmfile "$(cd .. && pwd)/global-hooks.cjs" --reporter=ndjson
	assert_success
	install_stdout="$output"

	# Effects landed: dep at 100.0.0 (global pin), is-positive at 1.0.0
	# (local override). pkg-with-1-dep@100.1.0 is the resolved version,
	# and the readPackage-injected deps appear in its virtual store
	# entry — neither was declared by the package itself.
	run jq -r .version "node_modules/.aube/@pnpm.e2e+pkg-with-1-dep@100.1.0/node_modules/@pnpm.e2e/dep-of-pkg-with-1-dep/package.json"
	assert_success
	assert_output 100.0.0
	run jq -r .version "node_modules/.aube/@pnpm.e2e+pkg-with-1-dep@100.1.0/node_modules/is-positive/package.json"
	assert_success
	assert_output 1.0.0

	# Two pnpm:hook readPackage ndjson records, in global-then-local
	# order. Same prefix (project root), distinct from (one per pnpmfile).
	# Avoid `mapfile` here — macOS still ships bash 3.2 in CI, so the
	# bash-4 builtin is unavailable. Pull individual records with sed
	# instead, which is portable across both runners.
	hook_logs=$(printf '%s\n' "$install_stdout" | jq -c 'select(.name == "pnpm:hook" and .hook == "readPackage")' 2>/dev/null)
	hook_count=$(printf '%s\n' "$hook_logs" | grep -c .)
	[ "$hook_count" -ge 2 ] || {
		echo "expected at least 2 pnpm:hook readPackage records, got $hook_count"
		echo "stdout was: $install_stdout"
		false
	}
	hook_log_0=$(printf '%s\n' "$hook_logs" | sed -n '1p')
	hook_log_1=$(printf '%s\n' "$hook_logs" | sed -n '2p')
	[ "$(printf '%s' "$hook_log_0" | jq -r '.message')" = 'is-positive pinned to 3.0.0' ]
	[ "$(printf '%s' "$hook_log_1" | jq -r '.message')" = 'is-positive pinned to 1.0.0' ]
	prefix0=$(printf '%s' "$hook_log_0" | jq -r '.prefix')
	prefix1=$(printf '%s' "$hook_log_1" | jq -r '.prefix')
	from0=$(printf '%s' "$hook_log_0" | jq -r '.from')
	from1=$(printf '%s' "$hook_log_1" | jq -r '.from')
	[ -n "$prefix0" ]
	[ -n "$from0" ]
	[ -n "$from1" ]
	[ "$prefix0" = "$prefix1" ]
	[ "$from0" != "$from1" ]
}

@test "pnpmfile: readPackage with shared workspace lockfile rewrites importer deps" {
	# Ported from pnpm/test/install/hooks.ts:661 ('pass readPackage with
	# shared lockfile'). Two-project workspace, each declaring
	# is-negative@1.0.0. The unconditional readPackage hook rewrites
	# *every* package's dependencies to `{ is-positive: '1.0.0' }`. aube
	# now fires the hook on importer manifests too (pnpm parity), so each
	# project's direct dep map is rewritten — node_modules/is-negative
	# drops out and node_modules/is-positive shows up.
	mkdir -p project-1 project-2
	cat >project-1/package.json <<'JSON'
{ "name": "project-1", "version": "1.0.0", "dependencies": { "is-negative": "1.0.0" } }
JSON
	cat >project-2/package.json <<'JSON'
{ "name": "project-2", "version": "1.0.0", "dependencies": { "is-negative": "1.0.0" } }
JSON
	cat >package.json <<'JSON'
{ "name": "workspace-root", "version": "0.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "*"
YAML
	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage: (pkg) => ({
      ...pkg,
      dependencies: {
        'is-positive': '1.0.0',
      },
    }),
  },
}
EOF

	run aube install
	assert_success
	assert_link_exists project-1/node_modules/is-positive
	assert_link_exists project-2/node_modules/is-positive
	assert_link_not_exists project-1/node_modules/is-negative
	assert_link_not_exists project-2/node_modules/is-negative
}

@test "pnpmfile: readPackage hook fires during aube remove in a workspace" {
	# Ported from pnpm/test/install/hooks.ts:580
	# ('readPackage hook in workspace during remove'). pnpm asserts the
	# hook-injected peerDep stays in the workspace lockfile after a
	# `pnpm remove <pkg>` from a sub-project, because remove triggers
	# a chained install that must wire the readPackage host the same
	# way `aube install` does. Substitution: pnpm's @pnpm.e2e/* fixtures
	# are mirrored, but for the regression guard we use the in-tree
	# is-odd / is-even / is-number leaves — pnpm's specific fixture
	# choice doesn't matter, only that a non-removed dep keeps the
	# hook-injected peerDep through the chained install.
	mkdir -p project-1
	cat >project-1/package.json <<'JSON'
{
  "name": "project-1",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1",
    "is-even": "1.0.0"
  }
}
JSON
	cat >package.json <<'JSON'
{ "name": "ws-root", "version": "0.0.0", "private": true }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - project-1
YAML
	# Hook injects a peerDep into is-odd. is-odd@3.0.1 is project-1's
	# direct dep; the injected peer survives whether or not is-even
	# is in the manifest, so we can scope the assertion to is-odd's
	# snapshot block.
	cat >.pnpmfile.cjs <<'EOF'
'use strict'
module.exports = {
  hooks: {
    readPackage (pkg) {
      if (pkg.name === 'is-odd') {
        pkg.peerDependencies = pkg.peerDependencies || {}
        pkg.peerDependencies['is-number'] = '*'
      }
      return pkg
    }
  }
}
EOF

	run aube install
	assert_success
	# Sanity: the hook fired during install — is-odd@3.0.1's snapshot
	# carries the injected `peerDependencies: is-number: '*'`. Scope
	# to the @3.0.1 snapshot block so the older transitive entry pulled
	# in by is-even@1.0.0 (is-odd@0.1.2) doesn't false-pass for us.
	run bash -c "awk '/^  is-odd@3.0.1:\$/{flag=1; next} /^  [^ ]/{flag=0} flag' aube-lock.yaml"
	assert_output --partial "peerDependencies"
	assert_output --partial "is-number"

	# Run remove from project-1 — this triggers a chained install. The
	# hook must fire on the new resolve, otherwise the rewritten
	# lockfile would drop the injected peerDep on the survivor.
	run bash -c 'cd project-1 && aube remove is-even'
	assert_success

	# The injected peerDep on the surviving is-odd@3.0.1 must still
	# be in the lockfile. Without the readPackage host wired into the
	# remove path's chained install, the rewrite would lose it.
	run bash -c "awk '/^  is-odd@3.0.1:\$/{flag=1; next} /^  [^ ]/{flag=0} flag' aube-lock.yaml"
	assert_output --partial "peerDependencies"
	assert_output --partial "is-number"
}
