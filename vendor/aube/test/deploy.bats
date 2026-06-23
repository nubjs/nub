#!/usr/bin/env bats

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

@test "aube deploy: copies selected workspace package into target and installs deps" {
	_setup_workspace_fixture

	run aube deploy --filter @test/lib ./out
	assert_success
	assert_output --partial "deployed @test/lib@1.0.0"

	# Package files were copied verbatim.
	assert_file_exists out/package.json
	assert_file_exists out/index.js

	# The install ran rooted at `out/`, so its deps are present.
	assert_dir_exists out/node_modules/is-odd

	# `out/` is standalone: no workspace/monorepo artifacts leaked in.
	[ ! -f out/pnpm-workspace.yaml ]
}

@test "aube deploy: subsets the source lockfile into the target" {
	_setup_workspace_fixture

	# Prime the source workspace so there's a real aube-lock.yaml to
	# subset. Without this, deploy falls back to a fresh install and
	# the new subset path isn't exercised.
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml

	run aube deploy --filter @test/lib ./out
	assert_success

	# Subset lockfile written alongside the staged package files. Keep
	# the format the source workspace uses (aube-lock.yaml here —
	# pnpm-lock.yaml, yarn.lock, etc. would carry through via
	# detect_existing_lockfile_kind).
	assert_file_exists out/aube-lock.yaml

	# Target is the sole importer, rekeyed to `.`. Source workspace
	# importers (`packages/lib`, `packages/app`) must not leak in —
	# otherwise a frozen install would see ghost entries.
	run grep -c "^  \.:" out/aube-lock.yaml
	assert_output "1"
	run grep -E "^  packages/" out/aube-lock.yaml
	assert_failure

	# Transitive closure pruned correctly: is-odd is in, anything only
	# @test/app needs is not. `@test/app`'s importer is pruned, so its
	# direct deps (e.g. the workspace link on @test/lib) don't appear.
	run grep -E "is-odd|is-number" out/aube-lock.yaml
	assert_success
	run grep -E "^  '?@test/app" out/aube-lock.yaml
	assert_failure
}

@test "aube deploy: bundles workspace: sibling deps as file: refs" {
	_setup_workspace_fixture

	# `@test/app` has `"@test/lib": "workspace:*"`. Deploy bundles the
	# sibling under `<target>/.aube-deploy-injected/<id>/` and rewrites
	# the spec to a relative `file:` pointer at the staged copy — the
	# install no longer needs to reach the registry for `@test/lib`,
	# which the offline fixture registry doesn't carry anyway.
	run aube deploy --filter @test/app ./out
	assert_success

	assert_file_exists out/package.json
	run node -e "console.log(require('./out/package.json').dependencies['@test/lib'])"
	assert_success
	assert_output "file:./.aube-deploy-injected/@test_lib"

	# Sibling files were copied into the staging directory.
	assert_file_exists out/.aube-deploy-injected/@test_lib/package.json
	assert_file_exists out/.aube-deploy-injected/@test_lib/index.js

	# Install resolved the bundled sibling — `@test/lib` is reachable
	# from the deployed package's node_modules tree.
	assert_dir_exists out/node_modules/@test/lib
}

@test "aube deploy: --offline reuses the store warmed by an earlier install" {
	_setup_workspace_fixture

	# Mirrors the multi-stage Dockerfile pattern: an earlier
	# `aube install` populates ~/.local/share/aube/store + the packument
	# cache, then `aube deploy --offline` reproduces a prod-only tree
	# without touching the network.
	run aube install
	assert_success

	# Force the registry URL to a host that doesn't resolve. If deploy
	# secretly hits the network despite --offline, the install pass
	# fails with a DNS error and this assertion catches it.
	echo "registry=http://aube-deploy-offline.invalid/" >.npmrc

	run aube deploy --filter @test/lib --offline ./out
	assert_success
	assert_output --partial "deployed @test/lib@1.0.0"
	assert_dir_exists out/node_modules/is-odd
}

@test "aube deploy: --offline and --prefer-offline conflict" {
	_setup_workspace_fixture

	run aube deploy --filter @test/lib --offline --prefer-offline ./out
	assert_failure
	assert_output --partial "cannot be used with"
}

@test "aube deploy: errors when --filter does not match a workspace package" {
	_setup_workspace_fixture

	run aube deploy --filter @test/does-not-exist ./out
	assert_failure
	# Error wraps across lines, so match a short substring that survives.
	assert_output --partial "did not match"
}

@test "aube deploy: workspace package without a version still deploys" {
	# Workspace-internal packages often have no `version` (nothing
	# publishes them). pnpm deploy accepts this; aube must match. The
	# deploy success log falls back to `0.0.0` so the format stays
	# uniform.
	mkdir -p psl
	printf "packages:\n  - psl\n" >aube-workspace.yaml
	printf '{"name":"psl"}\n' >psl/package.json

	run aube deploy --filter psl ./out
	assert_success
	assert_output --partial "deployed psl@0.0.0"
	assert_file_exists out/package.json
}

@test "aube deploy: refuses to deploy into a non-empty target" {
	_setup_workspace_fixture
	mkdir -p out
	echo hi >out/sentinel

	run aube deploy --filter @test/lib ./out
	assert_failure
	# miette wraps the rendered error at ~80 cols; long temp-dir paths can
	# split "not empty" at different points, so match a stable word.
	assert_output --partial "empty"
}

@test "aube deploy: glob filter fans out across every match" {
	_setup_workspace_fixture

	# `@test/*` matches both @test/lib and @test/app. Packages are
	# sorted by name before staging, so the plan is
	# [@test/app → out/app, @test/lib → out/lib]. Sibling bundling
	# now makes @test/app's deploy install successfully (the bundled
	# @test/lib copy serves the workspace dep without a registry
	# round-trip), so both targets land with installed trees.
	run aube deploy --filter "@test/*" ./out
	assert_success
	assert_file_exists out/lib/package.json
	assert_file_exists out/app/package.json
	# workspace: ref in out/app was rewritten to a `file:` pointer at
	# the staged sibling copy.
	run node -e "console.log(require('./out/app/package.json').dependencies['@test/lib'])"
	assert_success
	assert_output "file:./.aube-deploy-injected/@test_lib"
	# Each match got its own bundled sibling copy.
	assert_file_exists out/app/.aube-deploy-injected/@test_lib/package.json
	# @test/lib has no workspace deps of its own, so it bundled nothing.
	[ ! -d out/lib/.aube-deploy-injected ]
}

@test "aube deploy: multi-match refuses a non-empty target" {
	_setup_workspace_fixture
	mkdir -p out
	echo hi >out/sentinel

	run aube deploy --filter "@test/*" ./out
	assert_failure
	# miette wraps the rendered error at ~80 cols; long temp-dir paths can
	# split "not empty" at different points, so match a stable word.
	assert_output --partial "empty"
}

# Narrow @test/lib's publish surface to just package.json + index.js so
# `scripts/run.sh` and `tests/fixture.txt` are off the pack path. The
# `deployAllFiles` tests below rely on that exclusion.
_setup_lib_with_unpublished_files() {
	_setup_workspace_fixture
	# Rewrite package.json with a `files` field that excludes our extras.
	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "files": ["index.js"],
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF
	mkdir -p packages/lib/scripts packages/lib/tests
	echo "#!/bin/sh" >packages/lib/scripts/run.sh
	echo "fixture data" >packages/lib/tests/fixture.txt
}

@test "aube deploy: default honors pack's selection (files field excludes extras)" {
	_setup_lib_with_unpublished_files

	run aube deploy --filter @test/lib ./out
	assert_success

	assert_file_exists out/package.json
	assert_file_exists out/index.js
	# The `files` field restricted publish to index.js + package.json
	# (+ always-on files, but scripts/tests aren't in that set), so
	# deploy's default path must not copy them either.
	[ ! -f out/scripts/run.sh ]
	[ ! -f out/tests/fixture.txt ]
}

@test "aube deploy: deploy-all-files=true copies files pack's selection skips" {
	_setup_lib_with_unpublished_files
	# Project-level .npmrc is the source of truth for the deploy
	# command (read before any chdir into the target).
	echo "deploy-all-files=true" >.npmrc

	run aube deploy --filter @test/lib ./out
	assert_success

	# Publish surface is still there...
	assert_file_exists out/package.json
	assert_file_exists out/index.js
	# ...but so are the files that `files` / `.npmignore` would have
	# filtered.
	assert_file_exists out/scripts/run.sh
	assert_file_exists out/tests/fixture.txt
}

@test "aube deploy: deployAllFiles in pnpm-workspace.yaml honored" {
	_setup_lib_with_unpublished_files
	# Append the setting to the existing workspace yaml so we exercise
	# the workspaceYaml source path (camelCase alias).
	printf "\ndeployAllFiles: true\n" >>pnpm-workspace.yaml

	run aube deploy --filter @test/lib ./out
	assert_success

	assert_file_exists out/scripts/run.sh
	assert_file_exists out/tests/fixture.txt
}

@test "aube deploy: deploy-all-files=true still skips node_modules and .git" {
	_setup_lib_with_unpublished_files
	echo "deploy-all-files=true" >.npmrc

	# Pre-populate node_modules/ and .git/ inside the source package.
	# Both are filesystem cruft that must never end up in the deploy
	# target, even when "all files" is on.
	mkdir -p packages/lib/node_modules/ghost packages/lib/.git
	echo '{"name":"ghost"}' >packages/lib/node_modules/ghost/package.json
	echo "ref: refs/heads/main" >packages/lib/.git/HEAD

	run aube deploy --filter @test/lib ./out
	assert_success

	assert_file_exists out/scripts/run.sh
	[ ! -e out/node_modules/ghost ]
	[ ! -e out/.git ]
}

@test "aube deploy: bundles file: directory dep relative to the source workspace" {
	_setup_workspace_fixture

	# Add a sibling local-vendor directory and a `file:` ref into it
	# from @test/lib. The `file:../../local-vendor` spec resolves
	# relative to `packages/lib/` in the source workspace; without
	# bundling, that path would resolve wrong relative to the deploy
	# target.
	mkdir -p local-vendor
	cat >local-vendor/package.json <<'EOF'
{ "name": "vendored", "version": "0.0.1", "main": "index.js" }
EOF
	echo "module.exports = 'vendored'" >local-vendor/index.js
	# Append the file: dep to @test/lib's deps.
	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "dependencies": {
    "is-odd": "^3.0.1",
    "vendored": "file:../../local-vendor"
  }
}
EOF

	run aube deploy --filter @test/lib ./out
	assert_success

	# file: target was bundled, manifest rewritten to point at the
	# staged copy. The spec is relative to the deployed manifest,
	# not the original source workspace path.
	run node -e "console.log(require('./out/package.json').dependencies['vendored'])"
	assert_success
	assert_output "file:./.aube-deploy-injected/local-vendor"
	assert_file_exists out/.aube-deploy-injected/local-vendor/package.json
	assert_file_exists out/.aube-deploy-injected/local-vendor/index.js
	# Install resolved the bundled file: dep.
	assert_dir_exists out/node_modules/vendored
}

@test "aube deploy: --no-prod includes devDependencies in the deployed tree" {
	_setup_workspace_fixture

	# Add a workspace devDep to @test/lib. Default `--prod` deploy
	# would strip it from the manifest and lockfile; `--no-prod`
	# keeps it and bundles the sibling.
	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "@test/app": "workspace:*" }
}
EOF

	run aube deploy --no-prod --filter @test/lib ./out
	assert_success

	run node -e "console.log(JSON.stringify(require('./out/package.json').devDependencies))"
	assert_success
	assert_output --partial "@test/app"
	assert_output --partial "file:./.aube-deploy-injected/@test_app"
	assert_file_exists out/.aube-deploy-injected/@test_app/package.json
}

@test "aube deploy: --prod default strips devDependencies from manifest and tree" {
	_setup_workspace_fixture

	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "dependencies": { "is-odd": "^3.0.1" },
  "devDependencies": { "is-number": "^7.0.0" }
}
EOF

	run aube deploy --filter @test/lib ./out
	assert_success

	# devDependencies block is gone from the deployed manifest.
	run node -e "console.log(JSON.stringify(Object.keys(require('./out/package.json'))))"
	assert_success
	refute_output --partial "devDependencies"
	# is-number@^7 is the dev-only direct dep — it must not appear in
	# the deployed lockfile or node_modules. is-number@3 is reachable
	# through is-even/is-odd transitive chains and may legitimately
	# appear, so we match the dev-only major version exactly.
	run grep "is-number@7" out/aube-lock.yaml
	assert_failure
	# If is-number@3 ended up in node_modules as a transitive, that's
	# fine — we only assert the dev-only major (7.x) is absent.
	if [ -d out/node_modules/is-number ]; then
		run grep '"version": "7\.' out/node_modules/is-number/package.json
		assert_failure
	fi
}

@test "aube deploy: bundles workspace siblings recursively" {
	_setup_workspace_fixture

	# Add a third sibling `@test/core` that `@test/lib` depends on.
	mkdir -p packages/core
	cat >packages/core/package.json <<'EOF'
{ "name": "@test/core", "version": "0.0.1", "main": "index.js" }
EOF
	echo "module.exports = 'core'" >packages/core/index.js
	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "dependencies": {
    "is-odd": "^3.0.1",
    "@test/core": "workspace:*"
  }
}
EOF

	# Deploy @test/app, which depends on @test/lib (workspace:*),
	# which depends on @test/core (workspace:*). Both siblings must
	# bundle.
	run aube deploy --filter @test/app ./out
	assert_success

	assert_file_exists out/.aube-deploy-injected/@test_lib/package.json
	assert_file_exists out/.aube-deploy-injected/@test_core/package.json
	# Bundled @test/lib's manifest references @test/core via a
	# relative `file:` path inside the injected dir.
	run node -e "console.log(require('./out/.aube-deploy-injected/@test_lib/package.json').dependencies['@test/core'])"
	assert_success
	assert_output "file:../@test_core"
}

@test "aube deploy: deploy-all-files=true copies symlinked files via their target" {
	# Regression: `DirEntry::file_type()` uses lstat, so without
	# following file symlinks the walk would silently drop them —
	# contradicting the "copy every file" promise. Directory
	# symlinks are intentionally skipped (cycle risk) and covered
	# below.
	_setup_lib_with_unpublished_files
	echo "deploy-all-files=true" >.npmrc

	# Symlinked file: deploy target should receive the real content.
	echo "linked payload" >packages/lib/real.txt
	ln -s real.txt packages/lib/linked.txt

	# Symlinked directory pointing at a sibling inside the package.
	# Must NOT recurse into it (cycle / out-of-tree risk) — the
	# link itself is silently dropped. Contents still reach the
	# target through the direct `scripts/` walk.
	ln -s scripts packages/lib/scripts-alias

	run aube deploy --filter @test/lib ./out
	assert_success

	# Symlinked file: content copied verbatim (fs::copy follows).
	assert_file_exists out/linked.txt
	run cat out/linked.txt
	assert_output "linked payload"

	# Symlinked directory: neither the link nor a copy of the dir
	# under the alias name ended up in the target.
	[ ! -e out/scripts-alias ]
	# The real directory was walked directly, so its content is there.
	assert_file_exists out/scripts/run.sh
}

@test "aube deploy: resolves catalog: refs in the deployed manifest" {
	# Regression for #573: the deployed manifest carried `catalog:`
	# specifiers, but deploy didn't copy any workspace yaml into the
	# target. The install rooted at the target found no catalog
	# definitions and hard-failed with ERR_AUBE_UNKNOWN_CATALOG.
	# Deploy now rewrites `catalog:` to the concrete range from the
	# source workspace, so the artifact is self-contained.
	_setup_workspace_fixture
	# Define a default catalog and switch @test/lib's is-odd dep to a
	# bare `catalog:` reference. Use a `catalogs:` named block too so
	# both code paths in `discover_catalogs` are exercised.
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
catalog:
  is-odd: ^3.0.1
catalogs:
  test:
    is-odd: 3.0.1
EOF
	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "dependencies": {
    "is-odd": "catalog:"
  }
}
EOF

	run aube deploy --filter @test/lib ./out
	assert_success

	# `catalog:` was rewritten to the resolved range — no `catalog:`
	# leaked into the deployed manifest.
	run node -e "console.log(require('./out/package.json').dependencies['is-odd'])"
	assert_success
	assert_output "^3.0.1"
	# Install ran successfully against the rewritten manifest.
	assert_dir_exists out/node_modules/is-odd
}

@test "aube deploy: catalog rewrite uses named catalog when referenced" {
	_setup_workspace_fixture
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
catalogs:
  evens:
    is-odd: ^3.0.1
EOF
	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "dependencies": {
    "is-odd": "catalog:evens"
  }
}
EOF

	run aube deploy --filter @test/lib ./out
	assert_success
	run node -e "console.log(require('./out/package.json').dependencies['is-odd'])"
	assert_success
	assert_output "^3.0.1"
}

@test "aube deploy: undefined catalog reference errors with ERR_AUBE_UNKNOWN_CATALOG" {
	_setup_workspace_fixture
	# No catalog block at all — `is-odd: catalog:` cannot resolve.
	cat >packages/lib/package.json <<'EOF'
{
  "name": "@test/lib",
  "version": "1.0.0",
  "main": "index.js",
  "dependencies": {
    "is-odd": "catalog:"
  }
}
EOF

	run aube deploy --filter @test/lib ./out
	assert_failure
	assert_output --partial "ERR_AUBE_UNKNOWN_CATALOG"
	# Miette wraps narrow terminals, splitting `is-odd` across
	# `is-\nodd` on Linux CI — match on the catalog name + spec
	# (which sit on their own lines after wrap) instead.
	assert_output --partial "catalog \`default\`"
	assert_output --partial "catalog:"
}
