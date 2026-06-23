#!/usr/bin/env bats
#
# These tests exercise full workspace installs from the same fixture shape.
# Keep them in the serial pass so GNU parallel does not interleave their
# install/link assertions on hosted macOS.
#
# bats file_tags=serial

# Force within-file tests to run one at a time regardless of --jobs.
# shellcheck disable=SC2034
BATS_NO_PARALLELIZE_WITHIN_FILE=1

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_workspace_fixture() {
	cp -r "$PROJECT_ROOT/fixtures/workspace/"* .
}

@test "aube install: dependenciesMeta.injected copies workspace sibling" {
	_setup_workspace_fixture

	# Flip @test/lib from a symlinked workspace dep to an injected
	# one in the consumer (@test/app). After install the top-level
	# entry should be a symlink into a `.aube/...+inject_...`
	# directory that contains a real copy of lib's files — proving
	# the consumer sees a hard snapshot, not the source tree.
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@test/app",
		  "version": "1.0.0",
		  "dependencies": {
		    "@test/lib": "workspace:*",
		    "is-even": "^1.0.0"
		  },
		  "dependenciesMeta": {
		    "@test/lib": { "injected": true }
		  }
		}
	EOF

	run aube install
	assert_success

	# Top-level entry resolves through `.aube/.../+inject_.../node_modules/@test/lib`
	resolved="$(readlink -f packages/app/node_modules/@test/lib)"
	[[ "$resolved" == *"/node_modules/.aube/"*"+inject_"*"/node_modules/@test/lib" ]]

	# Injected copy is a real directory with lib's files present.
	assert_file_exists "$resolved/package.json"
	assert_file_exists "$resolved/index.js"

	# Source modification does NOT leak into the consumer — the
	# injected snapshot is decoupled from the source tree.
	echo "// MUTATED" >>packages/lib/index.js
	run grep -q MUTATED "$resolved/index.js"
	assert_failure

	# Injected copy still has a working resolver walk: is-odd is
	# reachable from the injected @test/lib via a sibling symlink.
	cd packages/app
	run node index.js
	assert_success
	assert_output --partial "isOdd(3): true"
}

@test "aube install: workspace with virtualStoreDir outside node_modules" {
	_setup_workspace_fixture

	# Regression: `link_workspace` wipes `root_nm` and then calls
	# `mkdirp(aube_dir)`. With the default layout the second call
	# recreates `root_nm` as an ancestor, so the subsequent
	# `create_dir_link` calls inside it succeed. When `aube_dir`
	# lives outside `root_nm` (custom override), that ancestor
	# effect disappears — without an explicit `mkdirp(root_nm)`
	# the workspace install fails on the first top-level symlink.
	# Name chosen to avoid colliding with the global store that BATS
	# rebases under $XDG_DATA_HOME/aube/store/v1/files — setting
	# virtual-store-dir=.aube-store would make us wipe our own cache.
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=.vstore-out
	EOF

	run aube install
	assert_success

	# Virtual store landed at the sibling path, and root_nm was
	# recreated so top-level workspace symlinks exist.
	assert_dir_exists .vstore-out
	run test -e node_modules/.aube
	assert_failure
	assert_dir_exists packages/app/node_modules
	assert_dir_exists packages/lib/node_modules

	# End-to-end resolution still works.
	cd packages/app
	run node index.js
	assert_success
	assert_output --partial "isOdd(3): true"
}

@test "aube install: dependenciesMeta.injected honors virtualStoreDir" {
	_setup_workspace_fixture

	# Same fixture as above, but relocate the virtual store so
	# apply_injected has to consult virtualStoreDir instead of
	# assuming `node_modules/.aube`. If the setting isn't threaded
	# into inject, either the injected entry lands at the wrong
	# path (breaking top-level resolution) or the sibling-symlink
	# pass quietly drops every registry dep (breaking the require
	# walk for is-odd).
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=node_modules/.custom-vs
	EOF

	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@test/app",
		  "version": "1.0.0",
		  "dependencies": {
		    "@test/lib": "workspace:*",
		    "is-even": "^1.0.0"
		  },
		  "dependenciesMeta": {
		    "@test/lib": { "injected": true }
		  }
		}
	EOF

	run aube install
	assert_success

	# Injected entry landed under the custom dir, not the default.
	assert_dir_exists node_modules/.custom-vs
	run test -e node_modules/.aube
	assert_failure

	resolved="$(readlink -f packages/app/node_modules/@test/lib)"
	[[ "$resolved" == *"/node_modules/.custom-vs/"*"+inject_"*"/node_modules/@test/lib" ]]

	# Sibling-symlink pass found the registry deps under the
	# custom dir — is-odd is reachable from the injected copy.
	cd packages/app
	run node index.js
	assert_success
	assert_output --partial "isOdd(3): true"
}

@test "aube install: workspace member without \`version\` field installs cleanly" {
	# Regression: aube errored with `workspace package <name> at <path>
	# has no \`version\` field` whenever any pnpm-workspace.yaml member
	# omitted version, even when no sibling depended on it. pnpm
	# permits unversioned members in this case (real-world: tuist's
	# `noora` design system, consumed by an external Mix toolchain).
	mkdir -p packages/standalone
	cat >package.json <<-'EOF'
		{ "name": "root-ws", "version": "0.0.0", "private": true }
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/standalone
	EOF
	cat >packages/standalone/package.json <<-'EOF'
		{
		  "name": "standalone",
		  "private": true,
		  "dependencies": {
		    "is-odd": "^3.0.1"
		  }
		}
	EOF

	run aube install
	assert_success

	assert_dir_exists packages/standalone/node_modules/is-odd
}

@test "aube install: sibling \`workspace:*\` links to unversioned member" {
	# Locks the "0.0.0" fallback path: when an unversioned member is
	# pinned via workspace:*, the resolver's wildcard branch matches
	# unconditionally and the linker creates the cross-package symlink.
	# Regression guard for the version-required check that errored
	# before any resolver work could happen.
	mkdir -p packages/lib packages/app
	cat >package.json <<-'EOF'
		{ "name": "root-ws", "version": "0.0.0", "private": true }
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/lib
		  - packages/app
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{ "name": "@test/lib", "private": true, "main": "index.js" }
	EOF
	echo "module.exports = 42;" >packages/lib/index.js
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@test/app",
		  "version": "1.0.0",
		  "private": true,
		  "dependencies": { "@test/lib": "workspace:*" }
		}
	EOF

	run aube install
	assert_success

	assert_link_exists packages/app/node_modules/@test/lib
	cd packages/app
	run node -e 'console.log(require("@test/lib"))'
	assert_success
	assert_output "42"
}

@test "aube install: detects workspace from pnpm-workspace.yaml" {
	_setup_workspace_fixture

	run aube install
	assert_success

	assert_dir_exists packages/lib/node_modules/is-odd
	assert_dir_exists packages/app/node_modules/@test/lib
}

@test "aube install: creates node_modules for each workspace package" {
	_setup_workspace_fixture

	run aube install
	assert_success

	# Root .aube should exist
	assert_dir_exists node_modules/.aube

	# Lib package should have is-odd
	assert_dir_exists packages/lib/node_modules/is-odd

	# App package should have is-even
	assert_dir_exists packages/app/node_modules/is-even
}

@test "aube install: workspace packages can require their deps" {
	_setup_workspace_fixture
	aube install

	# Lib can require is-odd
	run node -e "require('./packages/lib')"
	assert_success

	# App can require is-even
	cd packages/app
	run node -e "console.log(require('is-even')(4))"
	assert_success
	assert_output "true"
}

@test "aube install: workspace: protocol creates cross-package symlink" {
	_setup_workspace_fixture
	aube install

	# App's @test/lib should be a symlink to packages/lib
	assert_link_exists packages/app/node_modules/@test/lib
}

@test "aube install: app can require workspace lib package" {
	_setup_workspace_fixture
	aube install

	cd packages/app
	run node index.js
	assert_success
	assert_output --partial "isOdd(3): true"
	assert_output --partial "isEven(4): true"
}

@test "aube install: workspace writes shared lockfile" {
	_setup_workspace_fixture
	aube install

	assert_file_exists aube-lock.yaml

	# Lockfile should have multiple importers
	run grep "packages/app" aube-lock.yaml
	assert_success

	run grep "packages/lib" aube-lock.yaml
	assert_success
}

@test "aube install: workspace warm re-install still creates dep bins" {
	# Regression: the `AlreadyLinked` fast path in
	# `fetch_packages_with_root` made `package_indices` sparse on
	# warm installs, and `link_bins` / the workspace per-importer
	# bin loop silently `continue`d on missing entries. Since
	# `link_workspace` wipes `node_modules/.bin` on every run, the
	# second install left `.bin/` empty and broke every tool.
	mkdir -p packages/app
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	cat >packages/app/package.json <<-'EOF'
		{"name": "app", "version": "0.0.0", "dependencies": {"which": "6.0.1"}}
	EOF

	run aube install
	assert_success
	# `test -f` (regular file or symlink-to-regular-file) + `test -x`
	# proves the bin entry exists *and* is callable, whether it's a
	# plain symlink (`node-linker=hoisted` default) or the shim that
	# the isolated linker writes by default. Avoids comparing the
	# literal link-target string, which includes absolute-path noise
	# (`.../which/./dist/...`) that's not stable across machines.
	run test -f packages/app/node_modules/.bin/node-which
	assert_success
	run test -x packages/app/node_modules/.bin/node-which
	assert_success

	# Second install: the fix is specifically that this path
	# re-creates the bin entry even though the fetch phase took
	# the warm shortcut and didn't load a `PackageIndex` entry for
	# `which`.
	run aube install
	assert_success
	run test -f packages/app/node_modules/.bin/node-which
	assert_success
	run test -x packages/app/node_modules/.bin/node-which
	assert_success
}

# Set up a workspace where root and a child both declare `is-odd` as a
# direct dep at the same version — the minimal scenario that lets
# dedupeDirectDeps skip a per-importer symlink.
_setup_shared_direct_dep_workspace() {
	mkdir -p packages/app
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "app",
		  "version": "0.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
}

@test "aube install: dedupeDirectDeps=false (default) keeps per-importer dep symlink" {
	_setup_shared_direct_dep_workspace

	run aube install
	assert_success

	# Root symlink exists.
	run test -L node_modules/is-odd
	assert_success
	# Child symlink also exists — dedupe is off by default.
	run test -L packages/app/node_modules/is-odd
	assert_success
}

@test "aube install: dedupeDirectDeps=true skips child symlink when root has same version" {
	_setup_shared_direct_dep_workspace
	cat >.npmrc <<-'EOF'
		dedupe-direct-deps=true
	EOF

	run aube install
	assert_success

	# Root still has the dep as direct.
	run test -L node_modules/is-odd
	assert_success
	# Child's per-importer symlink is suppressed — resolution walks
	# up to the root-level symlink instead.
	run test -e packages/app/node_modules/is-odd
	assert_failure
	# Resolution from inside the child package still works because
	# node walks up from `packages/app/` and eventually reaches the
	# root `node_modules/is-odd` symlink.
	cd packages/app
	run node -e "console.log(typeof require('is-odd'))"
	assert_success
	assert_output "function"
}

@test "aube install: bare-semver range links to workspace package (yarn/npm/bun style)" {
	# yarn v1, npm, and bun workspaces let siblings pin each other with
	# a plain semver range — `"@test/lib": "1.0.0"` rather than
	# `"workspace:*"` — and link to the local workspace copy when name
	# + version match. Repro of excalidraw's monorepo, where inner
	# packages pin `@excalidraw/common: 0.18.0` and expect it to
	# resolve from the workspace rather than the registry (where it
	# does not exist).
	mkdir -p packages/app packages/lib
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "workspaces": ["packages/*"]
		}
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0"
		}
	EOF
	cat >packages/lib/index.js <<-'EOF'
		module.exports = "from-workspace";
	EOF
	# No `workspace:` protocol — bare semver, matches lib's version.
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@test/app",
		  "version": "1.0.0",
		  "dependencies": { "@test/lib": "1.0.0" }
		}
	EOF

	run aube install
	assert_success

	# The app's @test/lib entry links to the workspace copy, proving
	# the resolver preferred the local package over a registry lookup
	# (which would have failed since @test/lib has never been
	# published).
	assert_link_exists packages/app/node_modules/@test/lib
	resolved="$(readlink -f packages/app/node_modules/@test/lib)"
	[[ "$resolved" == *"/packages/lib" ]]

	cd packages/app
	run node -e "console.log(require('@test/lib'))"
	assert_success
	assert_output "from-workspace"
}

@test "aube install: caret range on workspace-package name links to local copy" {
	# Range form (not exact pin): workspace at 1.2.3 satisfies `^1.0.0`
	# in the consumer, so the short-circuit must still fire. Exercises
	# the non-trivial `version_satisfies` path — an exact-match test
	# alone wouldn't catch a regression in range parsing.
	mkdir -p packages/app packages/lib
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "workspaces": ["packages/*"]
		}
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{ "name": "@test/lib", "version": "1.2.3" }
	EOF
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@test/app",
		  "version": "1.0.0",
		  "dependencies": { "@test/lib": "^1.0.0" }
		}
	EOF

	run aube install
	assert_success

	assert_link_exists packages/app/node_modules/@test/lib
	resolved="$(readlink -f packages/app/node_modules/@test/lib)"
	[[ "$resolved" == *"/packages/lib" ]]
}

@test "aube install: workspace-name miss falls through to registry, not stolen" {
	# Workspace has @test/lib@1.0.0 but the consumer pins `^2.0.0`.
	# The short-circuit must NOT hijack the name just because it
	# matches a workspace package — version has to satisfy too.
	# Expected behavior: resolver falls through to the registry and
	# surfaces a registry-shaped error (the fixture registry does not
	# serve @test/lib). Guards against a regression where a workspace
	# miss silently links the wrong version.
	mkdir -p packages/app packages/lib
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "workspaces": ["packages/*"]
		}
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{ "name": "@test/lib", "version": "1.0.0" }
	EOF
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@test/app",
		  "version": "1.0.0",
		  "dependencies": { "@test/lib": "^2.0.0" }
		}
	EOF

	run aube install
	assert_failure
	# The error must be a registry error (we went past the workspace
	# branch), not a silent success that linked the wrong version.
	assert_output --partial "registry error for @test/lib"
	# And no symlink was created.
	run test -e packages/app/node_modules/@test/lib
	assert_failure
}

@test "aube install: workspace dep bins land in dependent's node_modules/.bin" {
	# discussion #352: workspace package A declares a `bin`, workspace
	# package B depends on A via `workspace:*`. pnpm symlinks A's bin
	# into B/node_modules/.bin so npm scripts in B can call it; aube
	# previously skipped these because workspace deps have no
	# `.aube/<dep_path>` materialization. Multiple consumers exercise
	# the per-install ws-package-json read cache.
	mkdir -p packages/app1 packages/app2 tools/dev/bin
	cat >package.json <<-'EOF'
		{"name": "root", "private": true, "version": "1.0.0"}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		  - tools/*
	EOF
	cat >tools/dev/package.json <<-'EOF'
		{
		  "name": "@test/dev",
		  "version": "1.0.0",
		  "bin": {"my-tool": "./bin/my-tool.mjs"}
		}
	EOF
	printf '#!/usr/bin/env node\nconsole.log("ok")\n' >tools/dev/bin/my-tool.mjs
	chmod +x tools/dev/bin/my-tool.mjs
	cat >packages/app1/package.json <<-'EOF'
		{
		  "name": "@test/app1",
		  "version": "1.0.0",
		  "devDependencies": {"@test/dev": "workspace:*"}
		}
	EOF
	cat >packages/app2/package.json <<-'EOF'
		{
		  "name": "@test/app2",
		  "version": "1.0.0",
		  "devDependencies": {"@test/dev": "workspace:*"}
		}
	EOF

	run aube install
	assert_success

	# Shim exists in each consumer's .bin and resolves to the
	# workspace package's bin script. Default `.bin/` entries under
	# the isolated linker are POSIX shims (so `extendNodePath` can
	# set NODE_PATH), which means `readlink -f` returns the shim's
	# own canonical path. Check the recorded target inside the shim
	# instead: the v1 marker comment carries the resolved relative
	# path for the prune / unlink pass to recover.
	for app in app1 app2; do
		bin="packages/$app/node_modules/.bin/my-tool"
		run test -e "$bin"
		assert_success
		if [ -L "$bin" ]; then
			target="$(readlink -f "$bin")"
		else
			# `aube-bin-shim v1 target=...` line embeds the
			# $basedir-relative path to the workspace file.
			target="$(grep -m1 'aube-bin-shim v1 target=' "$bin")"
		fi
		[[ "$target" == *"tools/dev/bin/my-tool.mjs" ]]
	done
}

@test "aube install: dedupeDirectDeps=true keeps child symlink when versions differ" {
	mkdir -p packages/app
	# Root pins is-number@3.0.0, child pins is-number@6.0.0 — both are
	# in the offline fixture registry and have no transitive deps.
	# Even with dedupe on, the version mismatch means the dep_paths
	# differ and the child symlink must be preserved, otherwise
	# requiring `is-number` from the child would resolve the wrong
	# copy.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "0.0.0",
		  "private": true,
		  "dependencies": { "is-number": "3.0.0" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "app",
		  "version": "0.0.0",
		  "dependencies": { "is-number": "6.0.0" }
		}
	EOF
	cat >.npmrc <<-'EOF'
		dedupe-direct-deps=true
	EOF

	run aube install
	assert_success

	run test -L node_modules/is-number
	assert_success
	# Child symlink must still exist — different resolved version.
	run test -L packages/app/node_modules/is-number
	assert_success
}

@test "aube install: sharedWorkspaceLockfile=false writes per-project lockfiles" {
	# Each workspace member gets its own lockfile next to its
	# package.json, and the workspace root — itself a project — gets its
	# own root lockfile too (pnpm parity). Resolver still runs once over
	# the whole workspace so workspace deps work.
	cat >package.json <<-'JSON'
		{ "name": "swl-root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		sharedWorkspaceLockfile: false
		packages:
		  - packages/*
	YAML

	mkdir -p packages/lib packages/app
	cat >packages/lib/package.json <<-'JSON'
		{ "name": "@test/lib", "version": "1.0.0", "dependencies": { "is-odd": "^3.0.1" } }
	JSON
	cat >packages/app/package.json <<-'JSON'
		{ "name": "@test/app", "version": "1.0.0", "dependencies": { "is-even": "^1.0.0" } }
	JSON

	run aube install
	assert_success

	# Each importer has its own lockfile…
	assert_file_exists packages/lib/aube-lock.yaml
	assert_file_exists packages/app/aube-lock.yaml
	# …and the root project gets its own lockfile too. pnpm parity: the
	# workspace root is itself a project (it ships a package.json), so
	# under sharedWorkspaceLockfile=false it writes a root lockfile
	# carrying only its own importer. This root declares no deps, so that
	# lockfile holds no member packages.
	assert_file_exists aube-lock.yaml
	run grep -qF "is-odd" aube-lock.yaml
	assert_failure
	run grep -qF "is-even" aube-lock.yaml
	assert_failure

	# Each per-project lockfile carries only its own importer (remapped
	# to `.`). Pull the `importers:` block out and grep within it so the
	# assertion isn't tripped by `time:` entries that survived the
	# subset (per `subset_to_importer`'s metadata-preserving contract).
	importers_lib="$(awk '/^importers:/,/^packages:/' packages/lib/aube-lock.yaml)"
	importers_app="$(awk '/^importers:/,/^packages:/' packages/app/aube-lock.yaml)"
	echo "$importers_lib" | grep -qF "is-odd:"
	echo "$importers_lib" | grep -vqF "is-even:" || false
	echo "$importers_app" | grep -qF "is-even:"
	echo "$importers_app" | grep -vqF "is-odd:" || false

	# node_modules still get linked correctly per package.
	assert_file_exists packages/lib/node_modules/is-odd/index.js
	assert_file_exists packages/app/node_modules/is-even/index.js
}

@test "aube install --filter: sharedWorkspaceLockfile=false writes only the selected member's lockfile" {
	# pnpm parity: `pnpm --filter <pkg> install` under
	# shared-workspace-lockfile=false rewrites only the selected
	# project's lockfile — it must not create or rewrite the workspace
	# root's lockfile (nor unrelated members') as a side effect of a
	# scoped command. Regression for the per-project write path
	# iterating every importer regardless of the active --filter.
	cat >package.json <<-'JSON'
		{ "name": "swl-root", "version": "0.0.0", "private": true, "dependencies": { "is-odd": "^3.0.1" } }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		sharedWorkspaceLockfile: false
		packages:
		  - packages/*
	YAML

	mkdir -p packages/lib packages/app
	cat >packages/lib/package.json <<-'JSON'
		{ "name": "@test/lib", "version": "1.0.0", "dependencies": { "is-odd": "^3.0.1" } }
	JSON
	cat >packages/app/package.json <<-'JSON'
		{ "name": "@test/app", "version": "1.0.0", "dependencies": { "is-even": "^1.0.0" } }
	JSON

	run aube install --filter @test/lib
	assert_success

	# Only the selected member's lockfile is written…
	assert_file_exists packages/lib/aube-lock.yaml
	# …the root (itself a project with its own deps) is left untouched…
	assert [ ! -e aube-lock.yaml ]
	# …and the unrelated member gets no lockfile either.
	assert [ ! -e packages/app/aube-lock.yaml ]
}

@test "aube install: sharedWorkspaceLockfile=false preserves each member's pnpm-lock.yaml" {
	# pnpm parity: when a workspace member already ships its own
	# pnpm-lock.yaml (the per-project layout pnpm writes under
	# shared-workspace-lockfile=false), aube must rewrite *that* file
	# in place rather than dropping a redundant aube-lock.yaml beside
	# it. Regression for per-project installs forcing the workspace
	# default format onto every member.
	cat >package.json <<-'JSON'
		{ "name": "swl-root", "version": "0.0.0", "private": true }
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		sharedWorkspaceLockfile: false
		packages:
		  - packages/*
	YAML

	mkdir -p packages/lib packages/app
	cat >packages/lib/package.json <<-'JSON'
		{ "name": "@test/lib", "version": "1.0.0", "dependencies": { "is-odd": "^3.0.1" } }
	JSON
	cat >packages/app/package.json <<-'JSON'
		{ "name": "@test/app", "version": "1.0.0", "dependencies": { "is-even": "^1.0.0" } }
	JSON

	# Seed each member with a pre-existing pnpm-lock.yaml so detection
	# has a foreign lockfile to preserve. `app` deliberately gets none
	# so it still exercises the workspace-default fallback.
	printf "lockfileVersion: '9.0'\n\nimporters:\n\n  .: {}\n" >packages/lib/pnpm-lock.yaml

	run aube install
	assert_success

	# lib already had pnpm-lock.yaml -> rewritten in place, no aube-lock.yaml.
	assert_file_exists packages/lib/pnpm-lock.yaml
	assert [ ! -e packages/lib/aube-lock.yaml ]
	# The rewritten pnpm lockfile carries the resolved dep.
	run grep -qF "is-odd" packages/lib/pnpm-lock.yaml
	assert_success

	# app had no lockfile -> falls back to the workspace default (aube-lock.yaml).
	assert_file_exists packages/app/aube-lock.yaml
	assert [ ! -e packages/app/pnpm-lock.yaml ]

	# The root is a project (it ships a package.json) but had no prior
	# lockfile, so it gets the workspace-default aube-lock.yaml carrying
	# only its own (empty) importer — no pnpm-lock.yaml beside it.
	assert_file_exists aube-lock.yaml
	assert [ ! -e pnpm-lock.yaml ]

	# node_modules still get linked correctly per package.
	assert_file_exists packages/lib/node_modules/is-odd/index.js
	assert_file_exists packages/app/node_modules/is-even/index.js
}
