#!/usr/bin/env bats
#
# Ported from pnpm/test/monorepo/index.ts.
# See test/PNPM_TEST_IMPORT.md for translation conventions.
#
# This file covers Phase 3 batch 1 — filter + `--filter` semantics for
# workspace commands. pnpm's monorepo suite is large (41 tests, 2026
# LOC); the batches in PNPM_TEST_IMPORT.md slice it by topic.

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# pnpm's `preparePackages` creates each package as a sibling subdir
# without writing a root package.json. aube requires a root manifest at
# the workspace root, so all of these fixtures add a private root
# package.json — matching the conventional aube workspace shape and
# keeping the tests focused on filter behavior, not manifest discovery.

_setup_no_match_workspace() {
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
	EOF
	mkdir project
	cat >project/package.json <<-'EOF'
		{"name": "project", "version": "1.0.0"}
	EOF
}

@test "aube list --filter=<no-match>: warns to stdout and exits 0" {
	# Ported from pnpm/test/monorepo/index.ts:31 ('no projects matched the filters').
	_setup_no_match_workspace

	run aube list --filter=not-exists
	assert_success
	assert_output --partial "No projects matched the filters in"
}

@test "aube list --filter=<no-match> --fail-if-no-match: exits 1" {
	# Ported from pnpm/test/monorepo/index.ts:31 (sub-case 2).
	_setup_no_match_workspace

	run aube list --filter=not-exists --fail-if-no-match
	assert_failure
	assert_output --partial "did not match"
}

@test "aube list --filter=<no-match> --parseable: silent stdout, exits 0" {
	# Ported from pnpm/test/monorepo/index.ts:31 (sub-case 3). Machine
	# consumers expect empty stdout on no-match — the warning is
	# suppressed when --parseable is requested.
	_setup_no_match_workspace

	run aube list --filter=not-exists --parseable
	assert_success
	assert_output ""
}

@test "aube list --filter=<no-match>: --format parseable / --format json suppress the warning" {
	# Regression: the no-match suppression must check the resolved
	# output format, not just the `--parseable` / `--json` shortcuts.
	# `--format parseable` and `--format json` carry the same
	# machine-readable contract — printing the human "No projects
	# matched..." message would corrupt downstream parsers.
	_setup_no_match_workspace

	run aube list --filter=not-exists --format parseable
	assert_success
	assert_output ""

	run aube list --filter=not-exists --format json
	assert_success
	assert_output ""

	run aube list --filter=not-exists --json
	assert_success
	assert_output ""
}

@test "aube --filter=...<pkg> run: dependents run after the seed (topological order)" {
	# Ported from pnpm/test/monorepo/index.ts:512
	# ('do not get confused by filtered dependencies when searching for
	# dependents in monorepo'). The scenario: project-2 is filtered with
	# `...project-2` so dependents (project-3, project-4) join the run,
	# but two unrelated workspace packages (unused-project-{1,2}) sit in
	# project-2's dep list and shouldn't perturb the dependent search.
	# Topological order requires project-2 to run BEFORE project-3 and
	# project-4 — they depend on it.
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF
	mkdir unused-project-1 unused-project-2 project-2 project-3 project-4
	cat >unused-project-1/package.json <<-'EOF'
		{"name": "unused-project-1", "version": "1.0.0"}
	EOF
	cat >unused-project-2/package.json <<-'EOF'
		{"name": "unused-project-2", "version": "1.0.0"}
	EOF
	cat >project-2/package.json <<-'EOF'
		{
		  "name": "project-2",
		  "version": "1.0.0",
		  "dependencies": {"unused-project-1": "1.0.0", "unused-project-2": "1.0.0"},
		  "scripts": {"test": "node -e \"process.stdout.write('printed by project-2')\""}
		}
	EOF
	cat >project-3/package.json <<-'EOF'
		{
		  "name": "project-3",
		  "version": "1.0.0",
		  "dependencies": {"project-2": "1.0.0"},
		  "scripts": {"test": "node -e \"process.stdout.write('printed by project-3')\""}
		}
	EOF
	cat >project-4/package.json <<-'EOF'
		{
		  "name": "project-4",
		  "version": "1.0.0",
		  "dependencies": {"project-2": "1.0.0", "unused-project-1": "1.0.0", "unused-project-2": "1.0.0"},
		  "scripts": {"test": "node -e \"process.stdout.write('printed by project-4')\""}
		}
	EOF

	cd project-2
	run aube --filter='...project-2' run test
	assert_success
	assert_output --partial "printed by project-2"
	assert_output --partial "printed by project-3"
	assert_output --partial "printed by project-4"

	# Topological order: project-2 (the seed) before its dependents.
	# Flatten the captured output so newlines in install banners don't
	# break the substring search.
	local flat="${output//$'\n'/ }"
	local p2_idx="${flat%%printed by project-2*}"
	local p3_idx="${flat%%printed by project-3*}"
	local p4_idx="${flat%%printed by project-4*}"
	[ "${#p2_idx}" -lt "${#p3_idx}" ]
	[ "${#p2_idx}" -lt "${#p4_idx}" ]
}

# pnpm's "directory filtering" test (monorepo/index.ts:1662) covers two
# sub-cases. Sub-case 1 (`--filter=./packages` matches nothing) is an
# aube divergence: aube's path selector is "at or under", so
# `./packages` already matches packages nested below it. pnpm v9
# changed this to require the explicit `/**` recursive glob, gated on a
# `legacyDirFiltering` workspace setting. aube does not implement that
# setting (see test/PNPM_TEST_IMPORT.md "Explicitly skipped"). Only the
# `./packages/**` sub-case ports cleanly.
@test "aube list --filter=./packages/**: matches every package under the directory" {
	# Ported from pnpm/test/monorepo/index.ts:1662 (sub-case 2).
	# `--depth=-1` is pnpm's spelling for "list project headers only,
	# no deps". project-1 has a real dep (is-odd) so this also locks
	# the contract that `--depth=-1` skips dep enumeration even when
	# the importer has deps to enumerate — the no-deps semantics is
	# distinct from `--depth=0` (which prints direct deps).
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - "**"
		  - "!store/**"
	EOF
	mkdir -p packages/project-1 packages/project-2
	cat >packages/project-1/package.json <<-'EOF'
		{
		  "name": "project-1",
		  "version": "1.0.0",
		  "dependencies": {"is-odd": "^3.0.1"}
		}
	EOF
	cat >packages/project-2/package.json <<-'EOF'
		{"name": "project-2", "version": "1.0.0"}
	EOF

	# Populate the lockfile so `list --parseable` has something to walk.
	run aube install
	assert_success

	run aube list --filter='./packages/**' --parseable --depth=-1
	assert_success
	# Filtered `--parseable` leads each importer with its absolute
	# directory path (matches the help-text contract in list.rs and
	# pnpm's `list --filter=… --parseable` shape). Each project gets
	# its own line ending with the package directory.
	assert_line --regexp '/packages/project-1$'
	assert_line --regexp '/packages/project-2$'
	# `--depth=-1` must NOT emit any dep records (project-1 owns
	# is-odd as a direct dep — make sure it doesn't leak).
	refute_output --partial "is-odd"

	# Sanity: with `--depth=0` (direct deps only) the same fixture
	# does emit project-1's direct dep, so the suppression above is
	# specific to `-1`, not a side effect of the filter.
	run aube list --filter='./packages/**' --parseable --depth=0
	assert_success
	assert_output --partial "is-odd"
}

@test "aube --filter=<pkg> --workspace-root run: includes the workspace root" {
	# Ported from pnpm/test/monorepo/index.ts:1581.
	# pnpm names the command `test`; aube routes the same lifecycle
	# script through `run test` so the assertion stays about workspace
	# selection, not lifecycle shortcut parsing.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('root-ran','')\"" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
	EOF
	mkdir project
	cat >project/package.json <<-'EOF'
		{
		  "name": "project",
		  "version": "1.0.0",
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('project-ran','')\"" }
		}
	EOF

	run aube --filter=project --workspace-root run test --no-install
	assert_success
	assert_file_exists root-ran
	assert_file_exists project/project-ran
}

@test "includeWorkspaceRoot=true: recursive run includes the workspace root" {
	# Ported from pnpm/test/monorepo/index.ts:1613.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('root-ran','')\"" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		includeWorkspaceRoot: true
	EOF
	mkdir project
	cat >project/package.json <<-'EOF'
		{
		  "name": "project",
		  "version": "1.0.0",
		  "scripts": { "test": "node -e \"require('fs').writeFileSync('project-ran','')\"" }
		}
	EOF

	run aube -r run test --no-install
	assert_success
	assert_file_exists root-ran
	assert_file_exists project/project-ran
}

@test "aube list --filter=<no-match> --workspace-root: can return the root only" {
	# Regression guard for callers that use the lower-level workspace
	# selector directly. Root inclusion must happen before empty-match
	# handling, so the root can be the whole selected set.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "packages/*"
	EOF
	mkdir -p packages/project
	cat >packages/project/package.json <<-'EOF'
		{"name": "project", "version": "1.0.0"}
	EOF
	run aube install --lockfile-only
	assert_success

	run aube list --filter=missing --workspace-root --parseable --depth=-1
	assert_success
	# pwd -P resolves the macOS /var -> /private/var symlink so the
	# expected path matches aube's canonicalized workspace root.
	assert_output "$(pwd -P)"
	refute_output --partial "No projects matched"
	refute_output --partial "packages/project"
}

# Helper: stand up the four-project workspace pnpm uses for the
# link-workspace-packages tests. Mirrors `preparePackages([{name, version}, …])`
# from pnpm's test harness — a flat layout under the cwd where each
# project owns a `package.json` with `name` + `version`.
_link_workspace_packages_fixture() {
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	mkdir project-1 project-2 project-3 project-4
	cat >project-1/package.json <<-'EOF'
		{"name": "project-1", "version": "1.0.0"}
	EOF
	cat >project-2/package.json <<-'EOF'
		{"name": "project-2", "version": "2.0.0"}
	EOF
	cat >project-3/package.json <<-'EOF'
		{"name": "project-3", "version": "3.0.0"}
	EOF
	cat >project-4/package.json <<-'EOF'
		{"name": "project-4", "version": "4.0.0"}
	EOF
}

# Ported from pnpm/test/monorepo/index.ts:112
# ('linking a package inside a monorepo with --link-workspace-packages
# when installing new dependencies'). Default `saveWorkspaceProtocol`
# is `rolling` in aube, matching what pnpm's test asserts: bare
# `aube add project-2` writes `workspace:^` (no version pin), and
# `--save-optional --no-save-workspace-protocol` opts the manifest
# back into a registry-style spec while the resolver still picks up
# the local sibling.
@test "aube add: --link-workspace-packages writes workspace:^ for siblings" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	run aube add project-2
	assert_success
	run aube add project-3 --save-dev
	assert_success
	run aube add project-4 --save-optional --no-save-workspace-protocol
	assert_success

	# Manifest assertions: rolling form for the default save and
	# save-dev flows, registry-style for the explicit opt-out.
	run grep -F '"project-2": "workspace:^"' package.json
	assert_success
	run grep -F '"project-3": "workspace:^"' package.json
	assert_success
	run grep -F '"project-4": "^4.0.0"' package.json
	assert_success

	# Each sibling resolved through the local workspace — node_modules
	# entries exist regardless of whether the spec form is workspace
	# or registry style.
	assert_link_exists node_modules/project-2
	assert_link_exists node_modules/project-3
	assert_link_exists node_modules/project-4
}

@test "aube add: linkWorkspacePackages=deep writes workspace:^ for siblings" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: deep
	EOF

	cd project-1
	run aube add project-2
	assert_success

	run grep -F '"project-2": "workspace:^"' package.json
	assert_success
	assert_link_exists node_modules/project-2
}

# Ported from pnpm/test/monorepo/index.ts:156
# ('linking a package inside a monorepo with --link-workspace-packages
# when installing new dependencies and save-workspace-protocol is
# "rolling"'). Aube's default already matches `rolling`, so this test
# pins the explicit setting form — `saveWorkspaceProtocol: rolling`
# in the workspace yaml — and confirms the same outcomes as the
# default-only port above.
@test "aube add: --link-workspace-packages with saveWorkspaceProtocol: rolling" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
		saveWorkspaceProtocol: rolling
	EOF

	cd project-1
	run aube add project-2
	assert_success
	run aube add project-3 --save-dev
	assert_success
	run aube add project-4 --save-optional --no-save-workspace-protocol
	assert_success

	run grep -F '"project-2": "workspace:^"' package.json
	assert_success
	run grep -F '"project-3": "workspace:^"' package.json
	assert_success
	run grep -F '"project-4": "^4.0.0"' package.json
	assert_success

	assert_link_exists node_modules/project-2
	assert_link_exists node_modules/project-3
	assert_link_exists node_modules/project-4
}

# Aube-side regression guard for `saveWorkspaceProtocol: true`: the
# pinned-version form (`workspace:^<version>`) is the third valid
# manifest shape, and pnpm's docs document it as the historic default
# even though pnpm's tests have moved to assert the rolling form. The
# test stays in the aube suite (no pnpm equivalent) because the three
# saveWorkspaceProtocol variants share one code path and a regression
# in any one of them would silently slip through the rolling-only
# ports above.
@test "aube add: saveWorkspaceProtocol: true pins workspace:^<version>" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
		saveWorkspaceProtocol: true
	EOF

	cd project-1
	run aube add project-2
	assert_success

	run grep -F '"project-2": "workspace:^2.0.0"' package.json
	assert_success
	assert_link_exists node_modules/project-2
}

# Regression guard for `aube add my-alias@project-2`: the
# `linkWorkspacePackages` eligibility block must skip aliased specs
# because `workspace:` resolves by manifest key, so writing
# `"my-alias": "workspace:^"` would point the resolver at a sibling
# named `my-alias` (which doesn't exist) and 404 on the registry
# fallback. With the skip in place the aliased spec falls through to
# the registry path — which we don't run end-to-end here (the
# offline registry doesn't host `project-2`), but the failure mode
# we want to prevent is the silent `workspace:^` write.
@test "aube add: aliased spec does NOT trigger linkWorkspacePackages workspace match" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	# Aliased to a name that doesn't match any sibling, but the
	# real name (`project-2`) does. Pre-fix this would write
	# `"my-alias": "workspace:^"`. Post-fix the spec falls
	# through to the registry path and fails — the success
	# criterion is that `package.json` does NOT carry a
	# `workspace:` entry for `my-alias`.
	run aube add my-alias@project-2
	# Either failure mode (registry 404) or success is acceptable;
	# the regression guard is the manifest assertion below.
	run grep -F '"my-alias": "workspace:' package.json
	assert_failure
}

# Regression guard for `aube add project-2@^1.0.0` when project-2
# is at version 2.0.0 in the workspace: the user's explicit range
# rules out the local sibling, so the spec must fall through to the
# registry path rather than silently writing a `workspace:^` link
# that resolves to an incompatible version. The bats offline
# registry doesn't host project-2 so the registry path 404s — the
# success criterion is purely that the manifest does NOT carry a
# `workspace:` entry for project-2.
@test "aube add: explicit range mismatching sibling does NOT trigger workspace link" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	# project-2 is at 2.0.0 in the fixture; ^1.0.0 doesn't satisfy.
	run aube add project-2@^1.0.0
	# Don't assert exit status — registry 404 is the expected
	# fall-through. The regression guard is the manifest.
	run grep -F '"project-2": "workspace:' package.json
	assert_failure
}

# Companion to the mismatch guard: `aube add project-2@^2.0.0`
# (which the sibling at 2.0.0 satisfies) MUST trigger the
# workspace match. This locks the satisfies-true branch so a
# regression can't silently skip every explicit-range add.
@test "aube add: explicit range satisfying sibling DOES trigger workspace link" {
	_link_workspace_packages_fixture
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	EOF

	cd project-1
	run aube add project-2@^2.0.0
	assert_success
	# Default rolling form, since the user-typed range is `^2.0.0`
	# (caret) and the eligible sibling matches.
	run grep -F '"project-2": "workspace:^"' package.json
	assert_success
	assert_link_exists node_modules/project-2
}

# Phase 3 batch 2 — workspace: protocol edge cases
#
# Ported from pnpm/test/monorepo/index.ts:1317
# ('linking the package's bin to another workspace package in a
# monorepo'). The pnpm test uses `manifestFormat: 'YAML'` to write
# package.yaml; aube reads package.json only (intentional divergence,
# see PNPM_TEST_IMPORT.md "Won't fix"), so the port uses package.json.
#
# The bin-link-on-fresh-install half is already covered by
# test/workspace.bats:530. The unique regression guard ported here is
# the frozen-lockfile branch: after wiping every node_modules tree and
# rerunning install with --frozen-lockfile, the workspace:* sibling's
# bin shim must reappear in the dependent's .bin/ — i.e. the linker
# rebuilds workspace bin shims off the lockfile alone, without
# re-resolving from manifests.
@test "aube install --frozen-lockfile: workspace:* bin shim is rematerialized" {
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - "**"
		  - "!store/**"
	EOF
	mkdir hello main
	cat >hello/package.json <<-'EOF'
		{
		  "name": "hello",
		  "version": "1.0.0",
		  "bin": "index.js"
		}
	EOF
	# pnpm's fixture writes a non-executable shebang-only file; the
	# bin shim aube creates is what makes it runnable, so the
	# inner script body doesn't matter for the regression guard.
	printf '#!/usr/bin/env node\n' >hello/index.js
	cat >main/package.json <<-'EOF'
		{
		  "name": "main",
		  "version": "2.0.0",
		  "dependencies": {"hello": "workspace:*"}
		}
	EOF

	run aube install
	assert_success
	# Sanity: the bin shim landed under main/node_modules/.bin.
	# The single-bin string form (`"bin": "index.js"`) is the edge
	# case under test — pnpm and aube derive the shim name from
	# the package name when the bin field is a bare string.
	run test -e main/node_modules/.bin/hello
	assert_success

	# Wipe every node_modules tree the way pnpm's test does and
	# reinstall via --frozen-lockfile. Include hello/node_modules
	# in the wipe so the linker can't fall back to a surviving
	# sibling tree as a shortcut — the lockfile must encode enough
	# information to rematerialize the bin shim without re-reading
	# hello's manifest fresh.
	rm -rf hello/node_modules main/node_modules node_modules

	run aube install --frozen-lockfile
	assert_success
	run test -e main/node_modules/.bin/hello
	assert_success
}

# Phase 3 batch 3 — shared-workspace-lockfile behavior
#
# Aube's `sharedWorkspaceLockfile` defaults to `true` (matching pnpm):
# the workspace records every importer's resolved graph in one root
# lockfile. Per-project lockfiles (`sharedWorkspaceLockfile: false`)
# are covered alongside the shared default in [test/workspace.bats:624]
# and [test/pnpm_savecatalog.bats:104]; this batch focuses on the
# default `true` shape — importer key layout, removal handling, and
# the no-packages workspace-yaml edge case.

@test "workspace: registry version wins when sibling does not satisfy the spec" {
	# Ported from pnpm/test/monorepo/index.ts:610 (the registry-fallback
	# half of 'shared-workspace-lockfile: installation with
	# --link-workspace-packages links packages even if they were
	# previously installed from registry'). The pnpm-side full flow
	# also relinks after a sibling's manifest version changes; the
	# more important regression aube needed first was that an
	# incompatible local sibling does NOT silently override the
	# registry-resolved version. Aube-side fix: the linker now gates
	# the workspace-link branch on whether the resolver actually
	# picked the sibling (no `LockedPackage` for the dep_path means
	# "workspace pick"); a resolved registry version takes the
	# registry-link branch even when a sibling shares the name.
	cat >package.json <<-'JSON'
		{ "name": "root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "**"
		  - "!store/**"
		linkWorkspacePackages: true
	YAML
	mkdir is-positive project
	cat >is-positive/package.json <<-'JSON'
		{ "name": "is-positive", "version": "3.0.0" }
	JSON
	# Pin to 2.0.0 — sibling at 3.0.0 does NOT satisfy. Aube must
	# fall back to the registry instead of silently linking the
	# incompatible local copy.
	cat >project/package.json <<-'JSON'
		{
		  "name": "project",
		  "version": "1.0.0",
		  "dependencies": { "is-positive": "2.0.0" }
		}
	JSON

	run aube install
	assert_success

	# project's is-positive must resolve to 2.0.0 (registry), not
	# 3.0.0 (the sibling). The symlink target shape distinguishes
	# the two: a registry pick goes through `node_modules/.aube/`,
	# a workspace pick goes straight up to the sibling directory.
	resolved="$(readlink project/node_modules/is-positive 2>/dev/null)"
	[[ "$resolved" == *"node_modules/.aube/is-positive@2.0.0/"* ]]
	run node -e "console.log(require('./project/node_modules/is-positive/package.json').version)"
	assert_success
	assert_output "2.0.0"

	# Lockfile recorded the registry version too — the project
	# importer's is-positive entry resolves to the registry version
	# `2.0.0` (not `link:../is-positive` which would point at the
	# 3.0.0 sibling). Inner awk extracts just the project block.
	run bash -c "awk '/^  project:\$/{flag=1; next} /^  [^ ]/{flag=0} flag' aube-lock.yaml"
	assert_output --partial "version: 2.0.0"
	refute_output --partial "link:../is-positive"
}

@test "workspace: bare semver satisfying the sibling still links it" {
	# Companion to the mismatch test above. Locks the satisfies-true
	# branch of the resolver/linker so a regression of the version-
	# satisfaction gate doesn't silently fall through to the registry
	# for every workspace dep. Aube already routed `workspace:*` /
	# `workspace:^` siblings through the linker correctly; this guards
	# the bare-semver path that the resolver promotes to a workspace
	# link when the version satisfies.
	cat >package.json <<-'JSON'
		{ "name": "root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "**"
		  - "!store/**"
	YAML
	mkdir is-positive project
	cat >is-positive/package.json <<-'JSON'
		{ "name": "is-positive", "version": "3.0.0" }
	JSON
	cat >project/package.json <<-'JSON'
		{
		  "name": "project",
		  "version": "1.0.0",
		  "dependencies": { "is-positive": "3.0.0" }
		}
	JSON

	run aube install
	assert_success

	# 3.0.0 satisfies the sibling — link target is the sibling dir,
	# not the virtual store.
	resolved="$(readlink project/node_modules/is-positive 2>/dev/null)"
	[[ "$resolved" != *"node_modules/.aube/"* ]]
	run node -e "console.log(require('./project/node_modules/is-positive/package.json').version)"
	assert_success
	assert_output "3.0.0"
}

@test "shared-workspace-lockfile: install inside a single-project workspace creates shared lockfile format" {
	# Ported from pnpm/test/monorepo/index.ts:901 ('shared-workspace-lockfile:
	# create shared lockfile format when installation is inside workspace').
	# Covers https://github.com/pnpm/pnpm/issues/1437. The workspace yaml
	# resolves the cwd as the only importer, and the lockfile must use
	# the shared format — `importers:` keyed by the importer's relative
	# path (`.`), plus the standard top-level `packages:` / `snapshots:`
	# blocks. Pnpm's redundant `'project'` entry in the packages glob is
	# preserved verbatim to keep the test isomorphic.
	cat >package.json <<-'JSON'
		{
		  "name": "project",
		  "version": "0.0.0",
		  "private": true,
		  "dependencies": { "is-positive": "1.0.0" }
		}
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "**"
		  - "project"
		  - "!store/**"
		sharedWorkspaceLockfile: true
	YAML

	run aube install
	assert_success
	assert_file_exists aube-lock.yaml

	# Shared lockfile shape: top-level `importers:` / `packages:` /
	# `snapshots:` map plus a `lockfileVersion:` line. The `.` importer
	# carries the resolved spec for the root project's only dep.
	run grep -F "lockfileVersion:" aube-lock.yaml
	assert_success
	importers="$(awk '/^importers:/,/^packages:/' aube-lock.yaml)"
	echo "$importers" | grep -qE "^  \.:"
	echo "$importers" | grep -qF "is-positive"
	echo "$importers" | grep -qF "specifier: 1.0.0"

	# Top-level `packages:` map carries the resolved registry tarball.
	run grep -F "is-positive@1.0.0:" aube-lock.yaml
	assert_success
	# Symlink under .aube confirms the install actually completed
	# (lockfile shape alone wouldn't catch a no-op write).
	assert_link_exists node_modules/is-positive
}

@test "shared-workspace-lockfile: -r install handles relative ../** packages glob" {
	# Ported from pnpm/test/monorepo/index.ts:996 ('shared-workspace-lockfile:
	# install dependencies in projects that are relative to the workspace
	# directory'). The pnpm-workspace.yaml lives in monorepo/workspace/
	# and references siblings via `../**`, so the importer keys in the
	# shared lockfile end up as relative `../package-1` / `../package-2`
	# rather than the more common `package-1`. Locks the contract that
	# aube preserves the relative path verbatim in the lockfile and
	# resolves the deps through the workspace link.
	#
	# Aube-side fix: `aube_workspace::expand_workspace_pattern` now
	# anchors the walk via lexical resolution of the literal prefix
	# (so `../**` starts from the parent dir), and uses `pathdiff`
	# rather than `strip_prefix` to render the importer key — both for
	# the matcher comparison inside the walker and for the install
	# pipeline's `manifests` builder. Without these, the parent-tree
	# siblings either weren't visited or the importer key landed as
	# an absolute path that the lockfile + linker couldn't agree on.
	mkdir -p monorepo/workspace monorepo/package-1 monorepo/package-2
	cat >monorepo/workspace/package.json <<-'JSON'
		{
		  "name": "root-package",
		  "version": "1.0.0",
		  "dependencies": {
		    "package-1": "1.0.0",
		    "package-2": "1.0.0"
		  }
		}
	JSON
	cat >monorepo/package-1/package.json <<-'JSON'
		{
		  "name": "package-1",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-positive": "1.0.0",
		    "package-2": "1.0.0"
		  }
		}
	JSON
	cat >monorepo/package-2/package.json <<-'JSON'
		{
		  "name": "package-2",
		  "version": "1.0.0",
		  "dependencies": { "is-negative": "1.0.0" }
		}
	JSON
	cat >monorepo/workspace/pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "../**"
		  - "!../store/**"
	YAML

	cd monorepo/workspace
	run aube -r install
	assert_success

	# Shared lockfile lands at the workspace dir (cwd), not at any
	# sibling. Importer keys carry the workspace-relative path
	# verbatim — including the leading `..` for siblings reached via
	# the parent-tree glob.
	assert_file_exists aube-lock.yaml
	importers="$(awk '/^importers:/,/^packages:/' aube-lock.yaml)"
	echo "$importers" | grep -qE "^  \.:"
	echo "$importers" | grep -qF "../package-1"
	echo "$importers" | grep -qF "../package-2"

	# Top-level deps from each importer materialize as symlinks into
	# the sibling working trees — package-1 and package-2 each get
	# their transitive deps wired up under their own node_modules/.
	# package-1 sees package-2 as a sibling via the workspace link.
	assert_link_exists ../package-1/node_modules/is-positive
	assert_link_exists ../package-1/node_modules/package-2
	assert_link_exists ../package-2/node_modules/is-negative
	# package-1's package-2 link reaches the sibling source dir, not
	# the virtual store.
	resolved_p2="$(readlink -f ../package-1/node_modules/package-2)"
	[ "$resolved_p2" = "$(cd ../package-2 && pwd -P)" ]
}

@test "shared-workspace-lockfile: removed-on-disk project drops out of shared lockfile" {
	# Ported from pnpm/test/monorepo/index.ts:1108 ('shared-workspace-lockfile:
	# entries of removed projects should be removed from shared lockfile').
	# Two-project workspace; after deleting one project's directory and
	# re-running install, the importer entry for the deleted project
	# must vanish and its previously-locked transitive (is-negative)
	# must drop out of the top-level packages: map too.
	#
	# Aube-side fix: `LockfileGraph::check_drift_workspace` now flags
	# a stale importer (lockfile key with no current manifest) as
	# `Stale`, so the warm-path short-circuit re-runs the resolver and
	# the rewritten lockfile drops the orphan importer + snapshot.
	cat >package.json <<-'JSON'
		{ "name": "ws-root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "**"
		  - "!store/**"
	YAML
	mkdir -p package-1 package-2
	cat >package-1/package.json <<-'JSON'
		{ "name": "package-1", "version": "1.0.0", "dependencies": { "is-positive": "1.0.0" } }
	JSON
	cat >package-2/package.json <<-'JSON'
		{ "name": "package-2", "version": "1.0.0", "dependencies": { "is-negative": "1.0.0" } }
	JSON

	run aube -r install
	assert_success
	assert_file_exists aube-lock.yaml
	importers="$(awk '/^importers:/,/^packages:/' aube-lock.yaml)"
	echo "$importers" | grep -qE "^  package-1:"
	echo "$importers" | grep -qE "^  package-2:"

	# Wipe package-2's directory entirely so the workspace glob no
	# longer matches it, then reinstall.
	rm -rf package-2

	run aube install
	assert_success
	importers_after="$(awk '/^importers:/,/^packages:/' aube-lock.yaml)"
	echo "$importers_after" | grep -qE "^  package-1:"
	# package-2's importer entry must be gone.
	if echo "$importers_after" | grep -qE "^  package-2:"; then
		echo "regression: package-2 importer still present after rm -rf" >&2
		echo "$importers_after" >&2
		false
	fi
	# Its lone transitive (is-negative) must drop out of the top-level
	# packages: map too — no other importer pulled it in. Scope the
	# grep to the packages: block so we don't false-positive on
	# importer-side specifier strings.
	run bash -c "awk '/^packages:/,/^snapshots:/' aube-lock.yaml | grep -F 'is-negative@1.0.0:'"
	assert_failure
	# is-positive (package-1's dep) survived the cleanup.
	run bash -c "awk '/^packages:/,/^snapshots:/' aube-lock.yaml | grep -F 'is-positive@1.0.0:'"
	assert_success
}

@test "shared-workspace-lockfile: pnpm-workspace.yaml without packages doesn't break a single-project install" {
	# Ported from pnpm/test/monorepo/index.ts:1148
	# ('shared-workspace-lockfile config is ignored if no
	# pnpm-workspace.yaml is found'). Pnpm's title is misleading —
	# the test actually creates a pnpm-workspace.yaml that contains
	# *only* `sharedWorkspaceLockfile: true` and no `packages:` glob,
	# then asserts that the regular single-project install still works.
	# Covers https://github.com/pnpm/pnpm/issues/1482. The aube-side
	# regression guard: presence of a config-only workspace yaml must
	# not trip the workspace-mode install path or refuse to write a
	# project lockfile.
	cat >package.json <<-'JSON'
		{
		  "name": "project",
		  "version": "0.0.0",
		  "private": true,
		  "dependencies": { "is-positive": "1.0.0" }
		}
	JSON
	# Workspace yaml carries only the setting — no `packages:` glob.
	cat >pnpm-workspace.yaml <<-'YAML'
		sharedWorkspaceLockfile: true
	YAML

	run aube install
	assert_success
	assert_link_exists node_modules/is-positive
	# Lockfile lands next to package.json — same as a non-workspace
	# install. Either shape (importer `.` only, or no importers block)
	# is acceptable; the regression guard is that the install worked
	# and the dep is reachable.
	assert_file_exists aube-lock.yaml
}

@test "shared-workspace-lockfile: aube -r remove drops the dep from every project + the lockfile" {
	# Ported from pnpm/test/monorepo/index.ts:1162
	# ('shared-workspace-lockfile: removing a package recursively').
	# `aube -r remove is-positive` strips the dep from every project
	# that declared it (project1, project2), silently skips the third
	# project that never had it, and prunes is-positive from the
	# shared lockfile.
	#
	# Aube-side fix: `remove::run_filtered` now pre-checks each
	# selected workspace project's manifest and skips ones with no
	# overlap, matching pnpm's recursive-remove tolerance — the prior
	# behavior hard-failed on the first project missing the dep.
	cat >package.json <<-'JSON'
		{ "name": "ws-root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "**"
		  - "!store/**"
		sharedWorkspaceLockfile: true
		linkWorkspacePackages: true
	YAML
	mkdir -p project1 project2 project3
	cat >project1/package.json <<-'JSON'
		{
		  "name": "project1",
		  "version": "1.0.0",
		  "dependencies": { "is-positive": "2.0.0" }
		}
	JSON
	cat >project2/package.json <<-'JSON'
		{
		  "name": "project2",
		  "version": "1.0.0",
		  "dependencies": { "is-negative": "1.0.0", "is-positive": "1.0.0" }
		}
	JSON
	cat >project3/package.json <<-'JSON'
		{ "name": "project3", "version": "1.0.0" }
	JSON

	run aube -r install
	assert_success

	run aube -r remove is-positive
	assert_success

	# project1 had only is-positive — its dependencies map should be
	# empty (or absent) after the remove.
	run jq -r '.dependencies // {} | keys | length' project1/package.json
	assert_success
	assert_output "0"

	# project2 keeps is-negative, loses is-positive.
	run jq -r '.dependencies | keys | sort | join(",")' project2/package.json
	assert_success
	assert_output "is-negative"

	# project3 never declared is-positive — `aube -r remove` silently
	# skipped it (no error, manifest untouched). Stronger guard than
	# checking just `.name`: assert no `dependencies` key was added,
	# so a regression where the skip pre-check misfires and `run` runs
	# anyway can't slip through by leaving an empty `dependencies: {}`.
	run jq -r 'has("dependencies") | not' project3/package.json
	assert_success
	assert_output "true"
	run jq -r '.name' project3/package.json
	assert_success
	assert_output "project3"

	# Shared lockfile dropped the is-positive snapshot — the only
	# importers that referenced it are gone (project1 had only
	# is-positive; project2's is-positive at 1.0.0 is also removed).
	# Scope to the top-level packages: map so we don't false-positive
	# on importer-side specifier strings that mention is-positive.
	run bash -c "awk '/^packages:/,/^snapshots:/' aube-lock.yaml | grep -F 'is-positive@'"
	assert_failure
	# is-negative survives in the lockfile (project2 still owns it).
	run bash -c "awk '/^packages:/,/^snapshots:/' aube-lock.yaml | grep -F 'is-negative@1.0.0:'"
	assert_success
}

@test "shared-workspace-lockfile: removing every workspace package still prunes the lockfile" {
	# Boundary case for the stale-importer pass: when the workspace
	# loses its LAST sub-package (every directory removed, glob narrowed
	# to nothing), `manifests` collapses to `[(".", root)]` — same
	# shape as a non-workspace install. Without the explicit
	# `is_workspace_install` flag, the gate would mistake this for a
	# non-workspace install and skip the prune. Locks the contract
	# that the orphan importer + snapshot drop out as soon as the
	# workspace yaml's glob no longer matches the on-disk directory.
	cat >package.json <<-'JSON'
		{ "name": "ws-root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "**"
		  - "!store/**"
	YAML
	mkdir -p package-1
	cat >package-1/package.json <<-'JSON'
		{ "name": "package-1", "version": "1.0.0", "dependencies": { "is-positive": "1.0.0" } }
	JSON

	run aube install
	assert_success
	importers="$(awk '/^importers:/,/^packages:/' aube-lock.yaml)"
	echo "$importers" | grep -qE "^  package-1:"

	# Wipe the only sub-package. Workspace yaml still exists, so this
	# remains a workspace install — but `find_workspace_packages`
	# returns empty and `manifests` collapses to just `.`.
	rm -rf package-1

	run aube install
	assert_success
	importers_after="$(awk '/^importers:/,/^packages:/' aube-lock.yaml)"
	# package-1's importer entry must be gone.
	if echo "$importers_after" | grep -qE "^  package-1:"; then
		echo "regression: package-1 importer still present after rm -rf" >&2
		echo "$importers_after" >&2
		false
	fi
	# Its lone transitive (is-positive) must drop out of the top-level
	# packages: map too.
	run bash -c "awk '/^packages:/,/^snapshots:/' aube-lock.yaml | grep -F 'is-positive@1.0.0:'"
	assert_failure
}

@test "aube -r remove: partial overlap (project has some named pkgs, not all) succeeds cleanly" {
	# Regression for the `.any()` pre-check semantic gap: when one
	# project declares only a subset of the named packages, the prior
	# pre-check passed and `run` then hard-failed on the first
	# missing package — leaving manifest writes half-applied. The fix
	# narrows the package list per-project so each `run` invocation
	# only sees packages actually present.
	cat >package.json <<-'JSON'
		{ "name": "ws-root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - "**"
		  - "!store/**"
	YAML
	mkdir -p only-a both-a-b only-b
	# project A declares only is-odd; B declares both; C declares only is-even.
	cat >only-a/package.json <<-'JSON'
		{
		  "name": "only-a",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	JSON
	cat >both-a-b/package.json <<-'JSON'
		{
		  "name": "both-a-b",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1", "is-even": "1.0.0" }
		}
	JSON
	cat >only-b/package.json <<-'JSON'
		{
		  "name": "only-b",
		  "version": "1.0.0",
		  "dependencies": { "is-even": "1.0.0" }
		}
	JSON

	run aube -r install
	assert_success

	# Remove both packages recursively. Each project sees a different
	# subset of the names — pre-fix this would error on whichever
	# project was processed first that lacked one of the names.
	run aube -r remove is-odd is-even
	assert_success

	# Every project's `dependencies` map is now empty (or absent).
	for proj in only-a both-a-b only-b; do
		run jq -r '.dependencies // {} | keys | length' "$proj/package.json"
		assert_success
		assert_output "0"
	done

	# Both transitives dropped from the shared lockfile's packages: map.
	run bash -c "awk '/^packages:/,/^snapshots:/' aube-lock.yaml | grep -E 'is-odd@|is-even@'"
	assert_failure
}
