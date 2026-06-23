#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube cat-file --help" {
	run aube cat-file --help
	assert_success
	assert_output --partial "Print a file from the global store"
}

@test "aube cat-index --help" {
	run aube cat-index --help
	assert_success
	assert_output --partial "Print the cached package index"
}

@test "aube cat-index errors when given bare name (no version)" {
	run aube cat-index is-odd
	assert_failure
	assert_output --partial "expected \`name@version\`"
}

@test "aube cat-index errors when given a scoped name without version" {
	run aube cat-index @babel/core
	assert_failure
	assert_output --partial "expected \`name@version\`"
}

@test "aube cat-index rejects trailing-@ typos (empty version)" {
	run aube cat-index lodash@
	assert_failure
	assert_output --partial "expected \`name@version\`"

	run aube cat-index @babel/core@
	assert_failure
	assert_output --partial "expected \`name@version\`"
}

@test "aube cat-index errors when the package isn't cached yet" {
	run aube cat-index totally-not-a-real-pkg@1.0.0
	assert_failure
	assert_output --partial "no cached index"
	assert_output --partial "aube fetch"
}

@test "aube cat-file errors when the hash isn't in the store" {
	run aube cat-file sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
	assert_failure
	assert_output --partial "no file with hash"
}

@test "aube cat-file rejects non-hex input (no path traversal)" {
	# Without validation this would have escaped the store root via
	# PathBuf::join's absolute-path behavior.
	run aube cat-file ../../../etc/passwd
	assert_failure
	assert_output --partial "invalid hash"
}

@test "aube cat-file rejects single-character hex (no panic on split_at)" {
	run aube cat-file a
	assert_failure
	assert_output --partial "invalid hash"
}

@test "aube cat-index prints cached index JSON after install" {
	_setup_basic_fixture

	run aube install
	assert_success

	# Basic fixture pins is-odd@3.0.1 — after install the index cache is
	# populated, so cat-index should find it.
	run aube cat-index is-odd@3.0.1
	assert_success
	assert_output --partial "index.js"
	assert_output --partial "hex_hash"
	assert_output --partial "store_path"
}

@test "aube cat-file prints file bytes for a hash in the cat-index output" {
	_setup_basic_fixture

	run aube install
	assert_success

	# Pull a hex hash out of the index JSON, then round-trip it through
	# cat-file. Any file in the package works; `package.json` is the most
	# stable choice across versions.
	run aube cat-index is-odd@3.0.1
	assert_success
	# Extract the hex_hash of the package.json entry via a small awk script.
	hash=$(aube cat-index is-odd@3.0.1 | awk '
		/"package.json":/ { in_pkg = 1; next }
		in_pkg && /hex_hash/ {
			gsub(/[",]/, "", $2); print $2; exit
		}
	')
	[ -n "$hash" ]

	run aube cat-file "$hash"
	assert_success
	# is-odd's package.json always has a "name" field.
	assert_output --partial '"name":'
	assert_output --partial "is-odd"
}

@test "aube find-hash --help" {
	run aube find-hash --help
	assert_success
	assert_output --partial "List packages whose cached index references"
}

@test "aube find-hash rejects non-hex input (no path traversal)" {
	run aube find-hash ../../../etc/passwd
	assert_failure
	assert_output --partial "invalid hash"
}

@test "aube find-hash errors when no package matches" {
	_setup_basic_fixture
	run aube install
	assert_success

	# Valid-looking hex that won't match any real entry.
	run aube find-hash 00000000000000000000000000000000
	assert_failure
	assert_output --partial "no package index references"
}

@test "aube find-hash --json exits non-zero on no match (but still prints [])" {
	_setup_basic_fixture
	run aube install
	assert_success

	# `--json` must be consistent with the text mode: missing hash is an
	# error, even though we still emit a parseable empty array on stdout
	# so pipelines into `jq` don't break.
	run aube find-hash --json 00000000000000000000000000000000
	assert_failure
	assert_output --partial "[]"
	assert_output --partial "no package index references"
}

@test "aube find-hash locates a file the basic fixture installed" {
	_setup_basic_fixture
	run aube install
	assert_success

	# Pull a hex hash out of is-odd's cached index, then round-trip it
	# through find-hash. The `is-odd@3.0.1\tindex.js` row must appear.
	hash=$(aube cat-index is-odd@3.0.1 | awk '
		/"index.js":/ { in_pkg = 1; next }
		in_pkg && /hex_hash/ {
			gsub(/[",]/, "", $2); print $2; exit
		}
	')
	[ -n "$hash" ]

	run aube find-hash "$hash"
	assert_success
	assert_output --partial "is-odd@3.0.1"
	assert_output --partial "index.js"
}

@test "aube find-hash --json emits a structured array" {
	_setup_basic_fixture
	run aube install
	assert_success

	hash=$(aube cat-index is-odd@3.0.1 | awk '
		/"index.js":/ { in_pkg = 1; next }
		in_pkg && /hex_hash/ {
			gsub(/[",]/, "", $2); print $2; exit
		}
	')
	[ -n "$hash" ]

	run aube find-hash --json "$hash"
	assert_success
	assert_output --partial '"name": "is-odd"'
	assert_output --partial '"version": "3.0.1"'
	assert_output --partial '"path": "index.js"'
}
