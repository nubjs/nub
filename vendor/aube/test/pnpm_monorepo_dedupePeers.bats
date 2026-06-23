#!/usr/bin/env bats
#
# Ported from pnpm/test/monorepo/dedupePeers.test.ts.
# Phase 3 batch 4 (PNPM_TEST_IMPORT.md): dedupePeers across workspace.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# Coverage focus: workspace-wide peer-context dedupe behavior. The four
# cases verify that (1) peer-bearing snapshots are shared across importers
# instead of duplicated, (2) `aube update` from a sub-project does not
# rewrite manifests of unrelated importers, (3) `--filter <pkg> --latest`
# only touches the filtered project's manifest, and (4) auto-installed
# peers preserve their `(peer@version)` suffix on the importer dep entry
# under `dedupePeerDependents=true`.
#
# Mutates dist-tags on the committed Verdaccio storage via add_dist_tag
# and restores them via `git checkout` in teardown — same serial pattern
# as test/pnpm_install_preferOffline.bats / test/pnpm_update.bats.
#
# Adaptation note: pnpm's tests assert on exact peer-version suffixes
# like `(peer-a@1.0.0)` because pnpm's registry-mock only carries the
# versions that `addDistTag` published at test time. aube's static
# Verdaccio mirror has every published version (peer-a 1.0.0/1.0.1,
# peer-c 1.0.0/1.0.1/2.0.0), and aube's range-resolver picks the highest
# matching, so the rendered peer suffix will be the highest-in-range, not
# the dist-tag latest. The assertions below preserve the behavioral
# guards (one shared snapshot, isolated manifest updates, peer suffix
# present on the importer entry) without pinning to specific peer
# versions that wouldn't survive an upstream republish.
#
# bats file_tags=serial

# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	if [ -n "${PROJECT_ROOT:-}" ]; then
		git -C "$PROJECT_ROOT" checkout -- \
			test/registry/storage/@pnpm.e2e/abc-parent-with-ab/package.json \
			test/registry/storage/@pnpm.e2e/abc/package.json \
			test/registry/storage/@pnpm.e2e/abc-grand-parent-with-c/package.json \
			test/registry/storage/@pnpm.e2e/peer-a/package.json \
			test/registry/storage/@pnpm.e2e/peer-b/package.json \
			test/registry/storage/@pnpm.e2e/peer-c/package.json \
			test/registry/storage/@pnpm.e2e/foo/package.json \
			test/registry/storage/@pnpm.e2e/bar/package.json \
			2>/dev/null || true
	fi
	_common_teardown
}

_require_registry() {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
}

@test "aube --filter=<pkg> add: deduplicates peer-bearing snapshots across the workspace" {
	# Ported from pnpm/test/monorepo/dedupePeers.test.ts:15
	# ('deduplicate packages that have peers, when adding new dependency
	#  in a workspace').
	#
	# Both projects end up with @pnpm.e2e/abc in their dep tree —
	# project-1 transitively through abc-parent-with-ab, project-2 via a
	# direct filter add. With dedupePeerDependents=true the workspace
	# lockfile must hold ONE abc snapshot key (with its peer suffix),
	# not two — even though abc reaches each project via a different
	# importer path.
	#
	# Adaptations from pnpm's test:
	# - pnpm asserts `depPaths.length == 8` and exact peer versions.
	#   aube's static mirror exposes more versions of peer-a/peer-c than
	#   pnpm's per-test addDistTag mock did, so the highest-in-range
	#   resolution picks different versions. The parity guard is the
	#   dedupe — one abc snapshot, not two — so we assert that directly
	#   via a count of snapshot keys.
	# - pnpm pins versions via addDistTag against a registry-mock.
	#   aube's range resolver picks the highest matching version
	#   regardless of dist-tag, so we pin abc-parent-with-ab to 1.0.0
	#   directly in project-1's manifest (versions 1.0.1 and 1.1.0 of
	#   abc-parent-with-ab in our mirror have a stripped-down dep map
	#   that wouldn't pull abc transitively).
	# - pnpm uses autoInstallPeers=false; we keep that to match.
	_require_registry

	cat >package.json <<-'JSON'
		{"name": "root", "version": "0.0.0", "private": true}
	JSON
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
		dedupePeerDependents: true
		autoInstallPeers: false
	EOF
	mkdir project-1 project-2
	cat >project-1/package.json <<-'JSON'
		{
		  "name": "project-1",
		  "version": "1.0.0",
		  "dependencies": {
		    "@pnpm.e2e/abc-parent-with-ab": "1.0.0",
		    "@pnpm.e2e/peer-c": "1.0.0"
		  }
		}
	JSON
	cat >project-2/package.json <<-'JSON'
		{"name": "project-2", "version": "1.0.0"}
	JSON

	run aube install
	assert_success
	run aube --filter=project-2 add '@pnpm.e2e/abc@1.0.0'
	assert_success

	# Dedupe parity: extract the snapshots section and count keys that
	# start with `'@pnpm.e2e/abc@1.0.0` — should be exactly one. The
	# packages: section also carries an `'@pnpm.e2e/abc@1.0.0'` entry
	# without a peer suffix, so we anchor the grep to lines that follow
	# the `snapshots:` header.
	run bash -c "awk '/^snapshots:/{f=1;next} /^[a-z]/{f=0} f' aube-lock.yaml | grep -cE \"^  '@pnpm\\\\.e2e/abc@1\\\\.0\\\\.0(\\\\(|':)\""
	assert_success
	assert_output "1"

	# The deduped abc snapshot key contains all three peer names —
	# preserves the bug guard from pnpm/issues/6154 (dedupe must not
	# strip peer suffixes). Versions are not pinned in the assertion
	# because aube's range resolver picks highest-in-range and that
	# changes when upstream republishes a peer.
	run bash -c "awk '/^snapshots:/{f=1;next} /^[a-z]/{f=0} f' aube-lock.yaml | grep -E \"^  '@pnpm\\\\.e2e/abc@1\\\\.0\\\\.0\\\\(.*peer-a@.*peer-b@.*peer-c@\""
	assert_success

	# project-2's importer entry references the deduped snapshot via the
	# same peer-suffixed version string.
	run bash -c "awk '/^  project-2:/{f=1;next} /^  [a-z]/{f=0} f' aube-lock.yaml | grep -E \"version: 1\\\\.0\\\\.0\\\\(.*peer-a@.*peer-b@.*peer-c@\""
	assert_success
}

@test "aube update from sub-project: does not rewrite other projects' manifests" {
	# Ported from pnpm/test/monorepo/dedupePeers.test.ts:55 ('partial
	# update in a workspace should work with dedupe-peer-dependents is
	# true').
	#
	# Both projects pin abc-grand-parent-with-c@^1.0.0. After install,
	# `cd project-2 && aube update` must rewrite ONLY project-2's
	# manifest (if at all) — project-1's manifest must not be touched.
	# pnpm's bug-guard is that the shared lockfile under
	# dedupePeerDependents doesn't accidentally cascade manifest
	# rewrites across the workspace.
	#
	# Aube's `updateRewritesSpecifier` (default true) bumps caret/tilde
	# specs to the highest matching version on `update`. Our mirror has
	# abc-grand-parent-with-c@1.0.0 and 1.0.1, so the rewrite goes from
	# `^1.0.0` to `^1.0.1`. project-1's manifest stays at `^1.0.0`.
	_require_registry

	# Match pnpm's test setup: addDistTag for the inner packages only
	# (abc-parent-with-ab, abc, peer-a/b/c), NOT for the grand-parent.
	# Bumping the grand-parent's `latest` to 1.0.0 would tell aube's
	# `update` to downgrade the lockfile entry (which the highest-in-
	# range install picks at 1.0.1) — that's a different behavior from
	# pnpm and not what this test is asserting on. Leaving the grand-
	# parent at its default `latest=1.0.1` lets `update` keep the same
	# version while triggering the manifest-specifier rewrite path.
	add_dist_tag '@pnpm.e2e/abc-parent-with-ab' latest 1.0.0
	add_dist_tag '@pnpm.e2e/abc' latest 1.0.0
	add_dist_tag '@pnpm.e2e/peer-a' latest 1.0.0
	add_dist_tag '@pnpm.e2e/peer-b' latest 1.0.0
	add_dist_tag '@pnpm.e2e/peer-c' latest 1.0.0

	cat >package.json <<-'JSON'
		{"name": "root", "version": "0.0.0", "private": true}
	JSON
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
		dedupePeerDependents: true
		autoInstallPeers: false
	EOF
	mkdir project-1 project-2
	cat >project-1/package.json <<-'JSON'
		{
		  "name": "project-1",
		  "version": "1.0.0",
		  "dependencies": {"@pnpm.e2e/abc-grand-parent-with-c": "^1.0.0"}
		}
	JSON
	cat >project-2/package.json <<-'JSON'
		{
		  "name": "project-2",
		  "version": "1.0.0",
		  "dependencies": {"@pnpm.e2e/abc-grand-parent-with-c": "^1.0.0"}
		}
	JSON

	run aube install
	assert_success

	# Bump a transitive's dist-tag to simulate a downstream republish
	# between installs. The grand-parent's range still allows 1.0.1
	# directly via the static mirror, so the update will pick it up.
	add_dist_tag '@pnpm.e2e/abc-parent-with-ab' latest 1.0.1

	# Run the update from inside project-2 via `bash -c` so the bats
	# parent cwd stays at the workspace root for the manifest grep
	# assertions below. (shellcheck SC2103 — using a subshell
	# wrapping `run` would lose the `$status`/`$output` set by run.)
	run bash -c "cd project-2 && aube update"
	assert_success

	# project-1's manifest is unchanged — `aube update` (non-recursive)
	# scopes manifest rewrites to the current importer.
	run grep -E '"@pnpm.e2e/abc-grand-parent-with-c": "\^1\.0\.0"' project-1/package.json
	assert_success

	# project-2's manifest got rewritten to the highest in-range match
	# (^1.0.1 is the new minimum since the resolver picks 1.0.1).
	run grep -E '"@pnpm.e2e/abc-grand-parent-with-c": "\^1\.0\.1"' project-2/package.json
	assert_success
}

@test "aube update --filter=<pkg> --latest <dep>: only the filtered project's manifest is touched" {
	# Ported from pnpm/test/monorepo/dedupePeers.test.ts:101
	# ('partial update --latest in a workspace should not affect other
	#  packages when dedupe-peer-dependents is true').
	# Covers https://github.com/pnpm/pnpm/issues/8877.
	#
	# Both projects pin foo@100.0.0 and bar@100.0.0. After install,
	# bumping latest on both and running `aube update --filter project-2
	# --latest @pnpm.e2e/foo` must rewrite ONLY project-2's foo
	# (project-1 unchanged; project-2's bar unchanged because it wasn't
	# named on the CLI).
	#
	# Substitution: pnpm uses foo@1.0.0/2.0.0 and bar@100.0.0/100.1.0.
	# Our mirror has foo@100.0.0/100.1.0 and bar@100.0.0/100.1.0; we
	# adjust the version line for foo accordingly. Behavioral assertion
	# is identical.
	_require_registry

	add_dist_tag '@pnpm.e2e/foo' latest 100.0.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.0.0

	cat >package.json <<-'JSON'
		{"name": "root", "version": "0.0.0", "private": true}
	JSON
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
		dedupePeerDependents: true
		autoInstallPeers: false
	EOF
	mkdir project-1 project-2
	cat >project-1/package.json <<-'JSON'
		{
		  "name": "project-1",
		  "version": "1.0.0",
		  "dependencies": {
		    "@pnpm.e2e/foo": "100.0.0",
		    "@pnpm.e2e/bar": "100.0.0"
		  }
		}
	JSON
	cat >project-2/package.json <<-'JSON'
		{
		  "name": "project-2",
		  "version": "1.0.0",
		  "dependencies": {
		    "@pnpm.e2e/foo": "100.0.0",
		    "@pnpm.e2e/bar": "100.0.0"
		  }
		}
	JSON

	run aube install
	assert_success

	add_dist_tag '@pnpm.e2e/foo' latest 100.1.0
	add_dist_tag '@pnpm.e2e/bar' latest 100.1.0

	run aube update --filter=project-2 --latest '@pnpm.e2e/foo'
	assert_success

	# project-1: both pins untouched.
	run grep -E '"@pnpm.e2e/foo": "100\.0\.0"' project-1/package.json
	assert_success
	run grep -E '"@pnpm.e2e/bar": "100\.0\.0"' project-1/package.json
	assert_success

	# project-2: only foo bumped to 100.1.0; bar untouched.
	run grep -E '"@pnpm.e2e/foo": "100\.1\.0"' project-2/package.json
	assert_success
	run grep -E '"@pnpm.e2e/bar": "100\.0\.0"' project-2/package.json
	assert_success
}

@test "dedupePeerDependents=true + autoInstallPeers=true: peer suffix propagates onto the importer row" {
	# Ported from pnpm/test/monorepo/dedupePeers.test.ts:159
	# ('peer dependents deduplication should not remove peer
	#  dependencies'). Covers https://github.com/pnpm/pnpm/issues/6154.
	#
	# Root project depends on @pnpm.e2e/abc-parent-with-missing-peers,
	# which in turn depends on @pnpm.e2e/abc — and abc declares peer
	# deps a/b/c. With autoInstallPeers=true those peers are
	# auto-installed. The pnpm-parity contract under
	# dedupePeerDependents=true: the (peer-a)(peer-b)(peer-c) suffix
	# propagates UP through the dep chain onto the root importer's
	# `version:` field for abc-parent-with-missing-peers, even though
	# the parent itself doesn't declare those peers — so the importer
	# row uniquely identifies the peer context resolved at install
	# time, matching pnpm-lock.yaml byte shape.
	#
	# Aube wires the propagation in `propagate_peer_suffixes_to_ancestors`
	# (a post-pass after the peer-context fixed-point loop converges).
	# This test guards both the pnpm-parity shape on the importer row
	# AND the original #6154 bug: dedupe must not strip peer suffixes
	# from the snapshot key or the dep edge.
	_require_registry

	cat >package.json <<-'JSON'
		{
		  "name": "project-1",
		  "version": "1.0.0",
		  "dependencies": {"@pnpm.e2e/abc-parent-with-missing-peers": "1.0.0"}
		}
	JSON
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "."
		  - "project-2"
		dedupePeerDependents: true
		autoInstallPeers: true
	EOF
	mkdir project-2
	cat >project-2/package.json <<-'JSON'
		{"name": "project-2", "version": "1.0.0"}
	JSON

	run aube install
	assert_success
	run aube --filter=project-2 add 'is-positive@1.0.0'
	assert_success

	# 1. The root importer's `version:` field for the parent dep
	#    propagates the (peer-a)(peer-b)(peer-c) suffix even though
	#    abc-parent-with-missing-peers doesn't declare those peers.
	#    Pnpm-parity assertion — matches the test 4 shape from
	#    pnpm/test/monorepo/dedupePeers.test.ts:191 (with peer
	#    versions adapted to aube's highest-in-range resolution).
	run grep -E "version: 1\\.0\\.0\\(.*peer-a@.*peer-b@.*peer-c@" aube-lock.yaml
	assert_success

	# 2. abc-parent-with-missing-peers's snapshot key carries the
	#    same propagated suffix. Without the propagation pass aube
	#    would have keyed it as `abc-parent-with-missing-peers@1.0.0`
	#    bare — which is the divergence we're closing.
	run bash -c "awk '/^snapshots:/{f=1;next} /^[a-z]/{f=0} f' aube-lock.yaml | grep -E \"^  '@pnpm\\\\.e2e/abc-parent-with-missing-peers@1\\\\.0\\\\.0\\\\(.*peer-a@.*peer-b@.*peer-c@\""
	assert_success

	# 3. abc's own snapshot key carries the (a)(b)(c) suffix — the
	#    self-suffix path. Bug guard from #6154 (dedupe must not drop
	#    peer suffixes from the package that declares them).
	run bash -c "awk '/^snapshots:/{f=1;next} /^[a-z]/{f=0} f' aube-lock.yaml | grep -E \"^  '@pnpm\\\\.e2e/abc@1\\\\.0\\\\.0\\\\(.*peer-a@.*peer-b@.*peer-c@\""
	assert_success

	# 4. peer-a, peer-b, peer-c are materialized in the virtual store.
	#    Behavioral install contract — auto-install-peers actually
	#    placed the peers, regardless of the lockfile-shape changes.
	assert [ -e node_modules/.aube ]
	run bash -c "ls node_modules/.aube | grep -E '^@pnpm\\.e2e\\+peer-a@'"
	assert_success
	run bash -c "ls node_modules/.aube | grep -E '^@pnpm\\.e2e\\+peer-b@'"
	assert_success
	run bash -c "ls node_modules/.aube | grep -E '^@pnpm\\.e2e\\+peer-c@'"
	assert_success
}
