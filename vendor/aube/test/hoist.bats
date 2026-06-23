#!/usr/bin/env bats

# `hoist`, `hoistPattern`, and `hoistWorkspacePackages` control pnpm's
# hidden modules tree and workspace-symlink behavior:
#
# - `hoist` (default true) + `hoistPattern` (default ["*"]) decide
#   which non-local packages get a `node_modules/.aube/node_modules/<name>`
#   symlink. That directory is the fallback Node finds via its
#   parent-directory walk when a package inside the virtual store
#   does `require('undeclared-dep')`.
# - `hoistWorkspacePackages` (default true) decides whether workspace
#   packages get their own top-level `node_modules/<ws-pkg>` symlink
#   in each importer.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "hoist default populates .aube/node_modules with transitive deps" {
	cat >package.json <<'JSON'
{
  "name": "hoist-default",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install
	assert_success
	# Default hoistPattern=["*"]: every non-local package in the graph
	# (direct deps and transitives) gets a hidden-hoist symlink.
	assert_link_exists node_modules/.aube/node_modules/is-odd
	assert_link_exists node_modules/.aube/node_modules/is-number
	# The user-visible root still contains only the direct dep.
	assert_link_exists node_modules/is-odd
	assert_not_exists node_modules/is-number
}

@test "hoist=false skips the hidden modules directory" {
	cat >package.json <<'JSON'
{
  "name": "hoist-off",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	echo 'hoist=false' >.npmrc
	run aube install
	assert_success
	# Hidden tree is skipped entirely.
	assert_not_exists node_modules/.aube/node_modules
	# Direct dep is still linked at the visible root.
	assert_link_exists node_modules/is-odd
}

@test "hoistPattern narrows the hidden hoist selection" {
	cat >package.json <<'JSON'
{
  "name": "hoist-pattern",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	echo 'hoist-pattern=is-number' >.npmrc
	run aube install
	assert_success
	# Only is-number matches the pattern; is-odd is excluded from
	# the hidden tree (it's still at the user-visible root as a
	# direct dep).
	assert_link_exists node_modules/.aube/node_modules/is-number
	assert_not_exists node_modules/.aube/node_modules/is-odd
}

@test "hoistPattern via pnpm-workspace.yaml supports negations" {
	cat >package.json <<'JSON'
{
  "name": "hoist-pattern-yaml",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
hoistPattern:
  - "*"
  - "!is-number"
YAML
	run aube install
	assert_success
	# Everything except is-number makes it into the hidden tree.
	assert_link_exists node_modules/.aube/node_modules/is-odd
	assert_not_exists node_modules/.aube/node_modules/is-number
}

@test "flipping hoist=true -> false wipes the previous hidden tree" {
	cat >package.json <<'JSON'
{
  "name": "hoist-toggle",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	# First install populates the hidden tree.
	run aube install
	assert_success
	assert_link_exists node_modules/.aube/node_modules/is-number
	# Flip hoist off and reinstall — the stale symlinks must go so
	# Node stops resolving phantom deps through them.
	echo 'hoist=false' >.npmrc
	run aube install
	assert_success
	assert_not_exists node_modules/.aube/node_modules
}

@test "aube prune preserves the hidden hoist tree" {
	# `aube prune` walks `.aube/` to remove stale dep_path entries.
	# The `.aube/node_modules/` directory isn't a dep_path — it's the
	# hidden-hoist tree — and prune must skip it. Without that guard,
	# every `aube prune` would wipe phantom-dep fallbacks between
	# installs.
	cat >package.json <<'JSON'
{
  "name": "prune-hoist",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install
	assert_success
	assert_link_exists node_modules/.aube/node_modules/is-number
	run aube prune
	assert_success
	assert_link_exists node_modules/.aube/node_modules/is-number
}

@test "switching isolated -> hoisted wipes the stale hidden tree" {
	cat >package.json <<'JSON'
{
  "name": "hoist-relink",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	# First install in isolated mode populates the hidden tree.
	run aube install
	assert_success
	assert_link_exists node_modules/.aube/node_modules/is-number
	# Switching to hoisted mode should sweep the hidden tree —
	# hoisted doesn't use `.aube/<dep>/` so a leftover
	# `.aube/node_modules/` would resolve phantom deps for any
	# stale `.aube/` entry the top-level cleanup preserved.
	run aube install --node-linker=hoisted
	assert_success
	assert_not_exists node_modules/.aube/node_modules
}

@test "shamefully-hoist heals a broken top-level symlink for a transitive dep" {
	# Regression companion to the install.bats "heals a broken top-level
	# symlink" case: that one covers link_all (direct deps), this one
	# covers hoist_remaining_into (indirect, promoted to the root via
	# shamefully-hoist / public-hoist-pattern). Both used
	# `symlink_metadata().is_ok()` as the "already in place" check and
	# both silently survived dangling symlinks before the fix.
	cat >package.json <<'JSON'
{
  "name": "hoist-heal",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	echo 'shamefully-hoist=true' >.npmrc
	mkdir -p node_modules
	# is-number is a transitive of is-odd; shamefully-hoist promotes it
	# to node_modules/is-number via hoist_remaining_into. Pre-seed that
	# path with a dangling symlink so link_all's selective cleanup
	# preserves it (every graph name is in root_dep_names when
	# shamefully_hoist is on) and the hoist pass is the one that must
	# reclaim it.
	ln -s /definitely/does/not/exist node_modules/is-number
	assert [ -L node_modules/is-number ]
	assert [ ! -e node_modules/is-number ]

	run aube install
	assert_success

	assert [ -L node_modules/is-number ]
	assert_file_exists node_modules/is-number/package.json
	run node -e "require('is-number')"
	assert_success
}

@test "hoistWorkspacePackages=false omits workspace symlinks" {
	mkdir -p packages/app packages/lib
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - 'packages/*'
hoistWorkspacePackages: false
YAML
	cat >package.json <<'JSON'
{
  "name": "ws-root",
  "version": "1.0.0"
}
JSON
	cat >packages/app/package.json <<'JSON'
{
  "name": "app",
  "version": "1.0.0",
  "dependencies": {
    "lib": "workspace:*"
  }
}
JSON
	cat >packages/lib/package.json <<'JSON'
{
  "name": "lib",
  "version": "1.0.0"
}
JSON
	run aube install
	assert_success
	# With hoistWorkspacePackages=false, the importer does NOT get
	# its `node_modules/lib` symlink. The workspace graph still
	# records the dep, but a plain require('lib') no longer resolves.
	assert_not_exists packages/app/node_modules/lib
}

@test "hoistWorkspacePackages default links workspace deps" {
	mkdir -p packages/app packages/lib
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - 'packages/*'
YAML
	cat >package.json <<'JSON'
{
  "name": "ws-root",
  "version": "1.0.0"
}
JSON
	cat >packages/app/package.json <<'JSON'
{
  "name": "app",
  "version": "1.0.0",
  "dependencies": {
    "lib": "workspace:*"
  }
}
JSON
	cat >packages/lib/package.json <<'JSON'
{
  "name": "lib",
  "version": "1.0.0"
}
JSON
	run aube install
	assert_success
	# Default: workspace packages get top-level symlinks in each
	# importer's node_modules/.
	assert_link_exists packages/app/node_modules/lib
}
