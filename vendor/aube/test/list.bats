#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Set up a fixture with both prod and dev deps so every filter path has
# something to show. is-odd has is-number as a transitive — handy for
# --depth tests.
_setup_mixed_fixture() {
	cat >package.json <<'JSON'
{
  "name": "list-test",
  "version": "1.2.3",
  "dependencies": {
    "is-odd": "^3.0.1"
  },
  "devDependencies": {
    "is-number": "^7.0.0"
  }
}
JSON
	run aube install
	assert_success
}

@test "aube list prints project header and grouped deps" {
	_setup_mixed_fixture
	run aube list
	assert_success
	assert_output --partial "list-test@1.2.3"
	assert_output --partial "dependencies:"
	assert_output --partial "is-odd 3.0.1"
	assert_output --partial "devDependencies:"
	assert_output --partial "is-number 7.0.0"
}

@test "aube ls is an alias for aube list" {
	_setup_mixed_fixture
	run aube ls
	assert_success
	assert_output --partial "is-odd 3.0.1"
}

@test "aube ll prints the list tree with --long metadata" {
	_setup_mixed_fixture
	run aube ll
	assert_success
	assert_output --partial "is-odd 3.0.1"
	# --long mode appends the virtual-store path; the plain `aube list`
	# output above never contains `.aube/`, so seeing it here proves the
	# hidden alias set long=true instead of falling through to default.
	assert_output --partial ".aube/"
}

@test "aube la matches aube ll byte-for-byte" {
	_setup_mixed_fixture
	run aube ll
	assert_success
	local ll_out="$output"
	run aube la
	assert_success
	[ "$output" = "$ll_out" ]
}

@test "aube recursive ll wrapper lists workspace importers" {
	cat >package.json <<'JSON'
{ "name": "root", "version": "0.0.0", "private": true }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'JSON'
{ "name": "a", "version": "1.0.0", "dependencies": { "is-odd": "3.0.1" } }
JSON
	cat >packages/b/package.json <<'JSON'
{ "name": "b", "version": "1.0.0", "dependencies": { "is-number": "7.0.0" } }
JSON

	run aube install
	assert_success

	run aube recursive ll
	assert_success
	assert_output --partial "a@1.0.0"
	assert_output --partial "b@1.0.0"
	assert_output --partial "is-odd 3.0.1"
	assert_output --partial "is-number 7.0.0"
}

@test "aube list --prod shows only production deps" {
	_setup_mixed_fixture
	run aube list --prod
	assert_success
	assert_output --partial "is-odd 3.0.1"
	refute_output --partial "devDependencies:"
	refute_output --partial "is-number 7.0.0"
}

@test "aube list -P is a short alias for --prod" {
	_setup_mixed_fixture
	run aube list -P
	assert_success
	assert_output --partial "is-odd"
	refute_output --partial "is-number 7.0.0"
}

@test "aube list --dev shows only devDependencies" {
	_setup_mixed_fixture
	run aube list --dev
	assert_success
	assert_output --partial "is-number 7.0.0"
	refute_output --partial "dependencies:
└── is-odd"
}

@test "aube list --depth=0 omits transitive deps" {
	_setup_mixed_fixture
	run aube list --depth=0
	assert_success
	# is-odd depends on is-number@6.0.0 (transitive). depth=0 should hide it.
	refute_output --partial "└── is-number 6.0.0"
}

@test "aube list --depth=2 includes transitives" {
	_setup_mixed_fixture
	run aube list --depth=2
	assert_success
	assert_output --partial "is-odd 3.0.1"
	# is-odd → is-number@6.0.0 (the transitive version, not the dev one at 7.0.0)
	assert_output --partial "is-number 6.0.0"
}

@test "aube list --depth=Infinity accepts 'Infinity' as alias for max" {
	_setup_mixed_fixture
	run aube list --depth=Infinity
	assert_success
	# Same content as --depth=2 for this small fixture
	assert_output --partial "is-odd 3.0.1"
}

@test "aube list --json emits a valid JSON array" {
	_setup_mixed_fixture
	run aube list --json
	assert_success
	# Round-trip through node to prove it parses and has the expected shape.
	# Use BATS_TEST_TMPDIR (per-test, isolated) rather than a shared /tmp
	# path so parallel runs (CI matrix, --jobs N) don't clobber each other.
	local json_path="$BATS_TEST_TMPDIR/aube-list-output.json"
	echo "$output" >"$json_path"
	JSON_PATH="$json_path" run node -e '
		const data = JSON.parse(require("fs").readFileSync(process.env.JSON_PATH, "utf8"));
		if (!Array.isArray(data)) throw new Error("not an array");
		const root = data[0];
		if (root.name !== "list-test") throw new Error("wrong name: " + root.name);
		if (!root.dependencies || !root.dependencies["is-odd"]) throw new Error("missing is-odd");
		if (!root.devDependencies || !root.devDependencies["is-number"]) throw new Error("missing is-number");
		console.log("ok");
	'
	assert_success
	assert_output --partial "ok"
}

@test "aube list --parseable emits tab-separated lines" {
	_setup_mixed_fixture
	run aube list --parseable
	assert_success
	# Each line should be <dep_path>\t<name>\t<version>
	run bash -c 'aube list --parseable | awk -F"\t" "NF != 3 { exit 1 }"'
	assert_success
	# Output includes the prod dep
	aube list --parseable | grep -q 'is-odd@3.0.1'
}

@test "aube list <pattern> filters by name prefix" {
	_setup_mixed_fixture
	run aube list is-odd
	assert_success
	assert_output --partial "is-odd 3.0.1"
	refute_output --partial "is-number 7.0.0"
}

@test "aube list walks up to the workspace root from a subpackage cwd" {
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	cat >package.json <<'JSON'
{ "name": "root", "version": "0.0.0", "private": true, "dependencies": { "is-odd": "3.0.1" } }
JSON
	mkdir -p packages/lib-a
	cat >packages/lib-a/package.json <<'JSON'
{ "name": "@scope/lib-a", "version": "1.0.0" }
JSON

	run aube install
	assert_success

	cd packages/lib-a
	run aube list
	assert_success
	refute_output --partial "No lockfile found"
	assert_output --partial "is-odd 3.0.1"
}

@test "aube list without a lockfile prints a friendly message" {
	echo '{"name":"empty","version":"1.0.0"}' >package.json
	run aube list
	assert_success
	assert_output --partial "No lockfile found"
}

@test "aube list with no dependencies prints (no dependencies)" {
	cat >package.json <<'JSON'
{ "name": "empty", "version": "1.0.0" }
JSON
	# Populate an empty lockfile by running install
	run aube install
	assert_success
	run aube list
	assert_success
	assert_output --partial "(no dependencies)"
}

@test "aube ll --long honors virtualStoreDir override" {
	# Regression: before this fix, `list --long` printed
	# `./node_modules/.aube/<entry>` regardless of the user's
	# configured virtualStoreDir, pointing at a path that didn't
	# exist on disk once the store was relocated.
	_setup_mixed_fixture_with_vstore() {
		cat >package.json <<-'JSON'
			{
			  "name": "list-vstore-test",
			  "version": "1.0.0",
			  "dependencies": { "is-odd": "^3.0.1" }
			}
		JSON
		cat >>.npmrc <<-'EOF'

			virtual-store-dir=node_modules/vstore
		EOF
		run aube install
		assert_success
	}
	_setup_mixed_fixture_with_vstore
	run aube ll
	assert_success
	assert_output --partial "is-odd 3.0.1"
	# Points at the real store path...
	assert_output --partial "(./node_modules/vstore/"
	# ...and not the default.
	refute_output --partial "(./node_modules/.aube/"
}

@test "aube ll --long uses absolute path when virtualStoreDir is outside modulesDir" {
	cat >package.json <<-'JSON'
		{
		  "name": "list-vstore-outside",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^3.0.1" }
		}
	JSON
	# Point at a directory genuinely outside the project root so the
	# relative form the helper would otherwise pick contains `..` and
	# falls back to the absolute display. `$PWD/.vstore-abs` would
	# *still* be a child of cwd — its `pathdiff` relativizes to
	# `.vstore-abs`, so the assertion has to match the same relative
	# path the helper emits for it (CI caught this).
	local outside_vstore
	outside_vstore="$(mktemp -d "$(dirname "$TEST_TEMP_DIR")/outside-vstore.XXXXXX")"
	trap 'rm -rf "$outside_vstore"' RETURN
	cat >>.npmrc <<-EOF

		virtual-store-dir=$outside_vstore
	EOF
	run aube install
	assert_success
	run aube ll
	assert_success
	assert_output --partial "($outside_vstore/"
}
