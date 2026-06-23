#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Cache dir resolves through aube-store::dirs::cache_dir, which honors
# $HOME (set by _common_setup) so each test gets an isolated cache.
_cache_dir() {
	echo "$HOME/.cache/aube/packuments-v1"
}

# Drop a fake corgi-cache file for `pkg` so list/delete/view have
# something to chew on without requiring network access.
_fake_cache_entry() {
	local pkg="$1"
	local safe="${pkg//\//__}"
	mkdir -p "$(_cache_dir)"
	cat >"$(_cache_dir)/${safe}.json" <<EOF
{
  "etag": "W/\"abc123\"",
  "last_modified": "Wed, 01 Jan 2025 00:00:00 GMT",
  "fetched_at": 1735689600,
  "packument": {
    "name": "${pkg}",
    "versions": {
      "1.0.0":  {"name": "${pkg}", "version": "1.0.0"},
      "9.0.0":  {"name": "${pkg}", "version": "9.0.0"},
      "10.0.0": {"name": "${pkg}", "version": "10.0.0"}
    },
    "dist-tags": {"latest": "10.0.0"}
  }
}
EOF
}

@test "aube cache --help" {
	run aube cache --help
	assert_success
	assert_output --partial "Inspect and manage the packument metadata cache"
	assert_output --partial "list-registries"
}

@test "aube cache list on an empty cache prints nothing" {
	run aube cache list
	assert_success
	assert_output ""
}

@test "aube cache list prints cached package names (decoded)" {
	_fake_cache_entry "lodash"
	_fake_cache_entry "@babel/core"
	run aube cache list
	assert_success
	assert_line "lodash"
	assert_line "@babel/core"
}

@test "aube cache list filters by glob pattern" {
	_fake_cache_entry "lodash"
	_fake_cache_entry "@babel/core"
	_fake_cache_entry "@babel/parser"
	run aube cache list "@babel/*"
	assert_success
	assert_line "@babel/core"
	assert_line "@babel/parser"
	refute_line "lodash"
}

@test "aube cache view summarizes a cached entry" {
	_fake_cache_entry "lodash"
	run aube cache view lodash
	assert_success
	assert_output --partial "lodash (corgi)"
	assert_output --partial "versions:      3"
	# Regression guard: must use semver ordering, not lexicographic
	# (otherwise "9.0.0" would beat "10.0.0").
	assert_output --partial "highest:       10.0.0"
	assert_output --partial "latest: 10.0.0"
	assert_output --partial "etag:          W/\"abc123\""
}

@test "aube cache view --json dumps the raw cache file" {
	_fake_cache_entry "lodash"
	run aube cache view --json lodash
	assert_success
	assert_output --partial "\"etag\": \"W/\\\"abc123\\\"\""
	assert_output --partial "\"fetched_at\": 1735689600"
}

@test "aube cache view errors on a cold cache" {
	run aube cache view totally-not-cached
	assert_failure
	assert_output --partial "no cached metadata"
}

@test "aube cache delete removes matching entries" {
	_fake_cache_entry "lodash"
	_fake_cache_entry "@babel/core"
	run aube cache delete "@babel/*"
	assert_success
	assert_output --partial "removed"
	# lodash should still be there
	run aube cache list
	assert_success
	assert_line "lodash"
	refute_line "@babel/core"
}

@test "aube cache delete errors when nothing matched" {
	_fake_cache_entry "lodash"
	run aube cache delete "@babel/*"
	assert_failure
	assert_output --partial "no cached packages matched"
}

@test "aube cache list-registries prints the default registry" {
	# _common_setup writes registry=$AUBE_TEST_REGISTRY into .npmrc when
	# the var is set, otherwise the built-in default applies.
	run aube cache list-registries
	assert_success
	assert_output --partial "default:"
}
