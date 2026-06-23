#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_catalog_workspace() {
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.1
		catalogs:
		  evens:
		    is-even: ^1.0.0
	EOF

	cat >package.json <<-'EOF'
		{
		  "name": "aube-test-catalogs",
		  "version": "0.0.0",
		  "private": true
		}
	EOF

	mkdir -p packages/lib packages/app

	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "main": "index.js",
		  "dependencies": {
		    "is-odd": "catalog:"
		  }
		}
	EOF
	echo "module.exports = require('is-odd');" >packages/lib/index.js

	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@test/app",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-even": "catalog:evens"
		  }
		}
	EOF
}

@test "aube install: catalog: resolves from default catalog" {
	_setup_catalog_workspace

	run aube install
	assert_success

	assert_dir_exists packages/lib/node_modules/is-odd
	assert_dir_exists packages/app/node_modules/is-even
}

@test "aube install: catalog resolves when run from a subpackage" {
	# Regression: `aube install` run from inside a monorepo subpackage
	# used to miss the root `pnpm-workspace.yaml` entirely because the
	# loader only peeked at the project root (the nearest package.json).
	# Now it walks up for the workspace yaml.
	_setup_catalog_workspace

	cd packages/lib
	run aube install
	assert_success

	# Linked inside the subpackage even though the catalog lives up
	# one directory in `pnpm-workspace.yaml`.
	assert_dir_exists node_modules/is-odd
}

@test "aube install: bun-style root workspaces.catalog resolves from a subpackage" {
	# Regression: bun / npm / yarn workspaces don't ship a
	# `pnpm-workspace.yaml`; the workspace root is identified by a
	# `workspaces` field in `package.json`. `aube install` from a
	# subpackage used to miss that root entirely (catalog discovery only
	# walked up looking for the yaml), so a `catalog:` ref defined in
	# the root's `workspaces.catalog` failed with `UnknownCatalog`.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "private": true,
		  "workspaces": {
		    "packages": ["packages/*"],
		    "catalog": { "is-odd": "^3.0.1" }
		  }
		}
	EOF
	mkdir -p packages/lib
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "catalog:" }
		}
	EOF

	cd packages/lib
	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install: root pnpm.catalog (no yaml) resolves from a subpackage" {
	# Same shape as the bun-style regression above, but the root carries
	# a plain string-array `workspaces` field plus a `pnpm.catalog`
	# block — common when migrating from npm / yarn to pnpm-style
	# catalogs without adopting `pnpm-workspace.yaml`.
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "private": true,
		  "workspaces": ["packages/*"],
		  "pnpm": { "catalog": { "is-odd": "^3.0.1" } }
		}
	EOF
	mkdir -p packages/lib
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "catalog:" }
		}
	EOF

	cd packages/lib
	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install: catalog: resolves from package.json workspaces.catalog" {
	# Bun-style: catalogs live inline under `workspaces.catalog` in
	# package.json. No pnpm-workspace.yaml needed.
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test-workspaces-catalog",
		  "version": "0.0.0",
		  "dependencies": {
		    "is-odd": "catalog:"
		  },
		  "workspaces": {
		    "packages": [],
		    "catalog": {
		      "is-odd": "^3.0.1"
		    }
		  }
		}
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install: catalog: resolves from package.json pnpm.catalog" {
	# pnpm-style catalogs declared inline in package.json under `pnpm`.
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test-pnpm-catalog",
		  "version": "0.0.0",
		  "dependencies": {
		    "is-odd": "catalog:"
		  },
		  "pnpm": {
		    "catalog": {
		      "is-odd": "^3.0.1"
		    }
		  }
		}
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install: named catalog from pnpm.catalogs" {
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test-pnpm-catalogs",
		  "version": "0.0.0",
		  "dependencies": {
		    "is-even": "catalog:evens"
		  },
		  "pnpm": {
		    "catalogs": {
		      "evens": {
		        "is-even": "^1.0.0"
		      }
		    }
		  }
		}
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules/is-even
}

@test "aube install: workspace yaml wins over package.json catalog" {
	# When both sources define the same entry, the workspace yaml wins
	# (see `discover_catalogs` precedence in crates/aube/src/commands/mod.rs).
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test-catalog-precedence",
		  "version": "0.0.0",
		  "dependencies": {
		    "is-odd": "catalog:"
		  },
		  "pnpm": {
		    "catalog": {
		      "is-odd": "^0.1.0"
		    }
		  }
		}
	EOF

	run aube install
	assert_success
	# Workspace yaml's ^3.0.1 wins, so the resolved range is 3.x.
	run node -e 'console.log(require("is-odd/package.json").version)'
	assert_success
	[[ "$output" =~ ^3\. ]]
}

@test "aube install: lockfile records catalogs section" {
	_setup_catalog_workspace
	aube install

	run grep -A2 '^catalogs:' aube-lock.yaml
	assert_success
	assert_output --partial "default"

	run grep -A2 "^  default:" aube-lock.yaml
	assert_success

	run grep "specifier: \\^3.0.1" aube-lock.yaml
	assert_success
}

@test "aube install: importer entries keep catalog: specifier" {
	_setup_catalog_workspace
	aube install

	# `lib`'s is-odd dep should record `specifier: 'catalog:'`, not the
	# resolved range.
	run grep -A1 "^      is-odd:" aube-lock.yaml
	assert_success
	assert_output --partial "catalog:"
}

@test "aube install: unknown catalog reference fails fast" {
	mkdir -p packages/lib
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0", "private": true }
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "dependencies": { "is-even": "catalog:" }
		}
	EOF

	run aube install
	assert_failure
	assert_output --partial "catalog"
}

@test "aube install: unused workspace catalog entries don't trigger drift" {
	# Regression: declared-but-unreferenced catalog entries used to be
	# flagged as drift on every run, breaking --frozen-lockfile and
	# forcing a re-resolve under FrozenMode::Prefer.
	mkdir -p packages/lib
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.1
		  is-even: ^1.0.0
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0", "private": true }
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "catalog:" }
		}
	EOF

	aube install
	# Frozen-mode reinstall must succeed: drift check should ignore the
	# unreferenced is-even entry rather than treating it as "added".
	run aube install --frozen-lockfile
	assert_success
}

@test "aube add: succeeds with an existing catalog: dep in package.json" {
	# Regression: add/remove/update used to build resolvers without
	# catalogs, so any catalog: ref in package.json hard-failed with
	# UnknownCatalogRef when those commands ran.
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test",
		  "version": "0.0.0",
		  "dependencies": {
		    "is-odd": "catalog:"
		  }
		}
	EOF

	run aube add is-even@^1.0.0
	assert_success
	assert_dir_exists node_modules/is-odd
	assert_dir_exists node_modules/is-even
}

@test "aube install: editing the catalog re-resolves the lockfile" {
	_setup_catalog_workspace
	aube install
	assert_file_exists aube-lock.yaml

	# Bump the default-catalog spec; the resolver must notice and re-write.
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.0
		catalogs:
		  evens:
		    is-even: ^1.0.0
	EOF

	run aube install
	assert_success

	run grep "specifier: \\^3.0.0" aube-lock.yaml
	assert_success
}

@test "aube add: catalogMode=prefer rewrites matching dep to catalog:" {
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test",
		  "version": "0.0.0"
		}
	EOF
	echo "catalogMode=prefer" >>.npmrc

	run aube add is-odd@^3.0.1
	assert_success

	# Manifest records the catalog reference, not the bare range.
	run grep "\"is-odd\": \"catalog:\"" package.json
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube add: catalogMode=prefer falls back on incompatible range" {
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0" }
	EOF
	echo "catalogMode=prefer" >>.npmrc

	# Range incompatible with the catalog entry falls back to manual —
	# catalog:'s range doesn't cover ^0.1.0, so prefer should not rewrite.
	run aube add is-odd@^0.1.0
	assert_success

	run grep "\"is-odd\": \"catalog:\"" package.json
	assert_failure
}

@test "aube add: catalogMode=strict errors on mismatched range" {
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0" }
	EOF
	echo "catalogMode=strict" >>.npmrc

	run aube add is-odd@^0.1.0
	assert_failure
	assert_output --partial "catalogMode=strict"
}

@test "aube add: catalogMode=strict rewrites when range is implicit" {
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0" }
	EOF
	echo "catalogMode=strict" >>.npmrc

	run aube add is-odd
	assert_success
	run grep "\"is-odd\": \"catalog:\"" package.json
	assert_success
}

@test "aube add: catalogMode=strict prints the catalog-resolved version" {
	# Regression: a bare `aube add <pkg>` used to print the version
	# `latest` resolved to, even when the catalog rewrite redirected
	# resolution to a different range. `is-odd`'s `latest` tag is
	# `3.0.1`, but the catalog here pins `^0.1.0` — the printed and
	# installed versions must both come from the catalog's range.
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^0.1.0
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0" }
	EOF
	echo "catalogMode=strict" >>.npmrc

	run aube add is-odd
	assert_success
	assert_output --partial "is-odd@0.1.2 (specifier: catalog:)"
	refute_output --partial "is-odd@3.0.1 (specifier: catalog:)"
}

@test "aube add: default manual mode writes the bare range" {
	cat >pnpm-workspace.yaml <<-'EOF'
		catalog:
		  is-odd: ^3.0.1
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0" }
	EOF

	run aube add is-odd@^3.0.1
	assert_success

	# Manual mode preserves the caller's spec verbatim — no catalog rewrite.
	run grep "\"is-odd\": \"catalog:\"" package.json
	assert_failure
	run grep "\"is-odd\":" package.json
	assert_success
}

@test "aube install: cleanupUnusedCatalogs drops unreferenced entries" {
	mkdir -p packages/lib
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.1
		  is-even: ^1.0.0
		catalogs:
		  evens:
		    is-even: ^1.0.0
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0", "private": true }
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "catalog:" }
		}
	EOF
	echo "cleanupUnusedCatalogs=true" >>.npmrc

	run aube install
	assert_success

	# The unreferenced default-catalog entry and the empty `evens`
	# named catalog must be pruned; the used entry survives.
	run grep "is-odd" pnpm-workspace.yaml
	assert_success
	run grep "is-even" pnpm-workspace.yaml
	assert_failure
	run grep "^catalogs:" pnpm-workspace.yaml
	assert_failure
}

@test "aube install: cleanupUnusedCatalogs=false leaves the workspace yaml alone" {
	mkdir -p packages/lib
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
		catalog:
		  is-odd: ^3.0.1
		  is-even: ^1.0.0
	EOF
	cat >package.json <<-'EOF'
		{ "name": "aube-test", "version": "0.0.0", "private": true }
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@test/lib",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "catalog:" }
		}
	EOF

	# Snapshot the file before so we can compare byte-for-byte.
	before="$(cat pnpm-workspace.yaml)"
	run aube install
	assert_success
	after="$(cat pnpm-workspace.yaml)"
	[ "$before" = "$after" ]
}

@test "aube install: overrides value of catalog: resolves through catalog" {
	# Regression: `"overrides": {"pkg": "catalog:"}` used to leak the
	# raw `catalog:` string through to the registry resolver because
	# the catalog rewrite only ran against the user's declared range,
	# not the override target. The override must resolve the catalog
	# indirection too.
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test-override-catalog",
		  "private": true,
		  "dependencies": {
		    "is-odd": "^0.1.0"
		  },
		  "overrides": {
		    "is-odd": "catalog:"
		  },
		  "pnpm": {
		    "catalog": {
		      "is-odd": "3.0.1"
		    }
		  }
		}
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
	# The declared range `^0.1.0` would pick 0.1.2; the override
	# through the catalog must promote to 3.0.1.
	run cat node_modules/is-odd/package.json
	assert_output --partial '"version": "3.0.1"'
}

@test "aube install: overrides value of catalog:<name> resolves through named catalog" {
	# Same shape as the default-catalog case above, but the override
	# targets a named catalog (`catalog:pinned`) — the resolver must
	# look up the `pinned` catalog, not `default`.
	cat >package.json <<-'EOF'
		{
		  "name": "aube-test-override-named-catalog",
		  "private": true,
		  "dependencies": {
		    "is-odd": "^0.1.0"
		  },
		  "overrides": {
		    "is-odd": "catalog:pinned"
		  },
		  "pnpm": {
		    "catalogs": {
		      "pinned": {
		        "is-odd": "3.0.1"
		      }
		    }
		  }
		}
	EOF

	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
	run cat node_modules/is-odd/package.json
	assert_output --partial '"version": "3.0.1"'
}
