#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube fetch --help" {
	run aube fetch --help
	assert_success
	assert_output --partial "Download lockfile dependencies"
}

@test "aube fetch downloads into store without creating node_modules" {
	_setup_basic_fixture

	run aube fetch
	assert_success
	assert_output --partial "Fetching 7 of 7"
	assert_output --partial "Fetched 7 packages"

	# Key property: no node_modules is created.
	run test -e node_modules
	assert_failure
}

@test "aube fetch works without package.json (Docker cache-warming case)" {
	_setup_basic_fixture
	rm package.json

	run aube fetch
	assert_success
	assert_output --partial "Fetched 7 packages"
	run test -e node_modules
	assert_failure
}

@test "aube fetch populates cache so subsequent install is warm" {
	_setup_basic_fixture

	run aube fetch
	assert_success

	# Now an install should pull everything from the cache.
	run aube -v install
	assert_success
	assert_output --partial "Packages: 7 cached, 0 fetched"

	assert_dir_exists node_modules/is-odd
	assert_dir_exists node_modules/is-even
}

@test "aube fetch re-populates store after wipe even with node_modules intact" {
	# Regression: the install fetch phase's `AlreadyLinked` fast
	# path used to fire inside `aube fetch`, which meant if a prior
	# `aube install` had built `node_modules/.aube/<dep>` but the
	# global aube store was then wiped (e.g. Docker layer caching,
	# where node_modules and the store live in different cached
	# layers), `aube fetch` would silently do nothing and leave the
	# store empty. The caller — `fetch_packages` — now opts out of
	# the shortcut so every package goes through `store.load_index`,
	# which detects the missing store file and re-downloads.
	_setup_basic_fixture

	local aube_store="$XDG_DATA_HOME/aube/store"

	# First install populates both node_modules and the store.
	run aube install
	assert_success
	assert_dir_exists node_modules/.aube
	# Isolated HOME + XDG_DATA_HOME means the store lives there.
	assert_dir_exists "$aube_store/v1/files"

	# Wipe the store but leave node_modules intact — the case the
	# `AlreadyLinked` shortcut would have silently broken. The
	# `v1/` removal takes the cached package indexes (under `v1/index`)
	# with it, so `load_index` is forced to fall through to a real
	# tarball fetch when the store file is missing.
	rm -rf "$aube_store"
	# Belt-and-suspenders: also clear the legacy XDG-cache index dir
	# in case the user is on an older aube that hadn't run the
	# one-shot in-store migration yet.
	rm -rf "$HOME/.cache/aube/index"

	# `aube fetch` must re-download + repopulate the store, not
	# short-circuit on the existing .aube symlinks.
	run aube fetch
	assert_success
	assert_dir_exists "$aube_store/v1/files"

	# Sanity check: the store should now contain at least one file.
	run bash -c "find '$aube_store/v1/files' -type f | head -1"
	assert_success
	refute_output ""
}

@test "aube fetch --prod skips devDependencies" {
	# import-npm fixture: 2 prod deps (@sindresorhus/is, is-odd) +
	# 1 transitive (is-number) + 1 dev dep (kind-of) = 4 total.
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube fetch --prod
	assert_success
	assert_output --partial "Fetching 3 of 4"
}

@test "aube fetch --dev fetches only devDependencies" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube fetch --dev
	assert_success
	assert_output --partial "Fetching 1 of 4"
}

@test "aube fetch --prod errors on yarn.lock without package.json" {
	# yarn.lock needs the manifest to classify direct deps. Silently
	# fetching zero packages would be confusing, so we error loudly.
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube fetch --prod
	assert_failure
	assert_output --partial "requires package.json"
	assert_output --partial "yarn.lock"
}

@test "aube fetch --dev errors on bun.lock without package.json" {
	cp "$PROJECT_ROOT/fixtures/import-bun/bun.lock" .

	run aube fetch --dev
	assert_failure
	assert_output --partial "requires package.json"
	assert_output --partial "bun.lock"
}

@test "aube fetch (default) works on yarn.lock without package.json" {
	# No --prod/--dev → we don't need the manifest for classification.
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube fetch
	assert_success
	assert_output --partial "Fetched 4 packages"
}

@test "aube fetch --prod and --dev conflict" {
	_setup_basic_fixture

	run aube fetch --prod --dev
	assert_failure
	assert_output --partial "cannot be used with"
}

@test "aube fetch errors when no lockfile is present" {
	cat >package.json <<'EOF'
{"name": "empty", "version": "1.0.0"}
EOF

	run aube fetch
	assert_failure
	assert_output --partial "no lockfile found"
}

@test "aube fetch errors when neither lockfile nor package.json present" {
	run aube fetch
	assert_failure
	assert_output --partial "no lockfile found"
}
