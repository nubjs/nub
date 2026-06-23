#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# is-odd is pinned to 0.1.2 in the fixture registry, with 3.0.1 also
# available as the `latest` dist-tag. That gives us a stale `current`
# against a known-good `latest` with zero network access.

_write_pkg_with_old_is_odd() {
	cat >package.json <<-'EOF'
		{
		  "name": "outdated-fixture",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "0.1.2" }
		}
	EOF
}

@test "aube outdated flags a stale dep with current/wanted/latest columns and exits 1" {
	_write_pkg_with_old_is_odd
	run aube install
	assert_success

	# pnpm-compat: exits with code 1 when any dep is outdated so CI
	# pipelines using `aube outdated || exit 1` keep working.
	run aube outdated
	assert_failure
	[ "$status" -eq 1 ]
	assert_output --partial "Package"
	assert_output --partial "Current"
	assert_output --partial "Wanted"
	assert_output --partial "Latest"
	assert_output --partial "is-odd"
	assert_output --partial "0.1.2"
	assert_output --partial "3.0.1"
}

@test "aube outdated --json emits a package-keyed object with dependencyType" {
	_write_pkg_with_old_is_odd
	run aube install
	assert_success

	run aube outdated --json
	# Exit 1 on drift, same as the table path.
	assert_failure
	[ "$status" -eq 1 ]
	assert_output --partial '"is-odd"'
	assert_output --partial '"current": "0.1.2"'
	assert_output --partial '"latest": "3.0.1"'
	# pnpm-compat: each entry carries a `dependencyType` field keyed
	# by the package.json section the dep came from.
	assert_output --partial '"dependencyType": "dependencies"'
}

@test "aube outdated reports nothing when everything is up to date" {
	cat >package.json <<-'EOF'
		{
		  "name": "outdated-fresh",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success

	run aube outdated
	assert_success
	assert_output --partial "up to date"
	refute_output --partial "is-odd  "
}

@test "aube outdated skips registry for package.json workspace deps" {
	cat >package.json <<'EOF'
{"workspaces":["sub"],"dependencies":{"happy-sunny-hippo":"workspace:"}}
EOF
	mkdir sub
	cat >sub/package.json <<'EOF'
{"name":"happy-sunny-hippo"}
EOF

	run aube install
	assert_success

	run aube outdated
	assert_success
	assert_output --partial "(no matching dependencies)"
	refute_output --partial "package not found"
}

@test "aube outdated --recursive reports workspace importers" {
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"1.0.0","dependencies":{"is-odd":"0.1.2"}}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"1.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF

	run aube install
	assert_success

	run aube outdated --recursive
	assert_failure
	assert_output --partial "Importer"
	assert_output --partial "a"
	assert_output --partial "is-odd"
	assert_output --partial "0.1.2"
	refute_output --partial "b  "
}

@test "aube recursive outdated wrapper reports workspace importers" {
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"1.0.0","dependencies":{"is-odd":"0.1.2"}}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"1.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF

	run aube install
	assert_success

	run aube recursive outdated
	assert_failure
	assert_output --partial "Importer"
	assert_output --partial "a"
	assert_output --partial "is-odd"
	assert_output --partial "0.1.2"
	refute_output --partial "b  "
}

@test "aube outdated --recursive includes the workspace root" {
	# Discussion #602: pnpm's `-r` skips the root unless
	# `include-workspace-root: true` is set. For aube's `outdated` we
	# default to including it because read-only audits that omit
	# root-level shared dev tooling feel broken.
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true,"dependencies":{"is-odd":"0.1.2"}}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	mkdir -p packages/a
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"1.0.0","dependencies":{"is-even":"0.1.0"}}
EOF

	run aube install
	assert_success

	run aube outdated --recursive
	assert_failure
	# Workspace package importer.
	assert_output --partial "is-even"
	# Root importer's own deps must show up too — pre-fix the `root`
	# importer was silently dropped from the matched set.
	assert_output --partial "is-odd"
}

@test "aube outdated -w retargets at the workspace root from a sub-package" {
	# Mirrors `pnpm -w outdated`: from a sub-package, `-w` must report
	# the root manifest's deps (not the sub-package's) regardless of
	# cwd. Without `-w`, the outdated check resolves the sub-package's
	# lockfile, which doesn't exist in shared-lockfile workspaces and
	# would emit "No lockfile found".
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true,"dependencies":{"is-odd":"0.1.2"}}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	mkdir -p packages/api
	cat >packages/api/package.json <<'EOF'
{"name":"api","version":"1.0.0","dependencies":{"is-even":"0.1.0"}}
EOF

	run aube install
	assert_success

	cd packages/api
	run aube outdated -w
	assert_failure
	# Root's `is-odd` shows as outdated; the sub-package's `is-even`
	# must not appear because we're targeting the root manifest.
	assert_output --partial "is-odd"
	refute_output --partial "is-even"
}

@test "aube outdated --filter limits workspace importers" {
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"1.0.0","dependencies":{"is-odd":"0.1.2"}}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"1.0.0","dependencies":{"is-even":"1.0.0"}}
EOF

	run aube install
	assert_success

	run aube --filter a outdated
	assert_failure
	assert_output --partial "a"
	assert_output --partial "is-odd"
	refute_output --partial "is-even"
}
