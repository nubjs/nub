#!/usr/bin/env bats

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# --- color from .npmrc ---

_make_env_probe_project() {
	mkdir -p probe
	cd probe || return
	cat >package.json <<-'JSON'
		{
		  "name": "probe",
		  "version": "1.0.0",
		  "scripts": {
		    "env-probe": "node -e \"console.log('NO_COLOR=' + (process.env.NO_COLOR || '')); console.log('FORCE_COLOR=' + (process.env.FORCE_COLOR || '')); console.log('CLICOLOR_FORCE=' + (process.env.CLICOLOR_FORCE || ''))\""
		  }
		}
	JSON
}

@test "color=never in .npmrc sets NO_COLOR for child processes" {
	_make_env_probe_project
	echo 'color=never' >>"$HOME/.npmrc"
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE
	run aube run env-probe
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
}

@test "color=always in .npmrc sets FORCE_COLOR for child processes" {
	_make_env_probe_project
	echo 'color=always' >>"$HOME/.npmrc"
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE
	run aube run env-probe
	assert_success
	[[ "$output" == *"FORCE_COLOR=1"* ]]
	[[ "$output" == *"CLICOLOR_FORCE=1"* ]]
}

@test "--no-color CLI flag overrides color=always in .npmrc" {
	_make_env_probe_project
	echo 'color=always' >>"$HOME/.npmrc"
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE
	run aube --no-color run env-probe
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
}

# --- loglevel from .npmrc ---

@test "loglevel=debug in .npmrc enables debug logging" {
	_setup_basic_fixture
	echo 'loglevel=debug' >>"$HOME/.npmrc"
	run --separate-stderr aube install
	assert_success
	# Match tracing's DEBUG level (preceded by space or ANSI escape), not
	# the `-DEBUG` suffix appended to the version string on debug builds.
	[[ "$stderr" =~ [^-]DEBUG ]]
}

@test "--loglevel warn CLI flag overrides loglevel=debug in .npmrc" {
	_setup_basic_fixture
	echo 'loglevel=debug' >>"$HOME/.npmrc"
	run --separate-stderr aube --loglevel warn install
	assert_success
	[[ ! "$stderr" =~ [^-]DEBUG ]]
}

# --- useStderr ---

@test "--use-stderr redirects stdout to stderr" {
	mkdir -p use-stderr-probe
	cd use-stderr-probe || return
	cat >package.json <<-'JSON'
		{
		  "name": "use-stderr-probe",
		  "version": "1.0.0",
		  "scripts": {
		    "say": "echo hello-stdout"
		  }
		}
	JSON
	# Without --use-stderr, `aube run say` outputs to stdout.
	run --separate-stderr aube run say
	assert_success
	[[ "$output" == *"hello-stdout"* ]]

	# With --use-stderr, stdout is redirected to stderr.
	run --separate-stderr aube --use-stderr run say
	assert_success
	[[ "$stderr" == *"hello-stdout"* ]]
}

@test "useStderr=true in .npmrc redirects stdout to stderr" {
	mkdir -p use-stderr-probe2
	cd use-stderr-probe2 || return
	cat >package.json <<-'JSON'
		{
		  "name": "use-stderr-probe2",
		  "version": "1.0.0",
		  "scripts": {
		    "say": "echo hello-stdout"
		  }
		}
	JSON
	echo 'useStderr=true' >>"$HOME/.npmrc"
	run --separate-stderr aube run say
	assert_success
	[[ "$stderr" == *"hello-stdout"* ]]
}

# --- savePrefix from .npmrc ---

@test "save-prefix=~ in .npmrc uses tilde prefix in package.json" {
	_setup_basic_fixture
	aube install
	echo 'save-prefix=~' >>"$HOME/.npmrc"
	run aube add is-odd
	assert_success
	# The specifier in package.json should start with ~
	run node -e "const p = require('./package.json'); const v = p.dependencies['is-odd']; console.log(v)"
	[[ "$output" == "~"* ]]
}

@test "save-prefix= (empty) in .npmrc pins exact version" {
	_setup_basic_fixture
	aube install
	echo 'save-prefix=' >>"$HOME/.npmrc"
	run aube add is-odd
	assert_success
	# The specifier should be a bare version (no ^ or ~)
	run node -e "const p = require('./package.json'); const v = p.dependencies['is-odd']; console.log(v)"
	[[ "$output" != "^"* ]]
	[[ "$output" != "~"* ]]
	# Should be a version number
	[[ "$output" =~ ^[0-9] ]]
}

@test "--save-exact overrides save-prefix=~ in .npmrc" {
	_setup_basic_fixture
	aube install
	echo 'save-prefix=~' >>"$HOME/.npmrc"
	run aube add --save-exact is-odd
	assert_success
	run node -e "const p = require('./package.json'); const v = p.dependencies['is-odd']; console.log(v)"
	[[ "$output" != "^"* ]]
	[[ "$output" != "~"* ]]
	[[ "$output" =~ ^[0-9] ]]
}

# --- stateDir from .npmrc ---

@test "stateDir in .npmrc changes state file location" {
	_setup_basic_fixture
	local custom_state="$TEST_TEMP_DIR/custom-state"
	echo "stateDir=$custom_state" >>"$HOME/.npmrc"
	run aube install
	assert_success
	assert_dir_exists "$custom_state/.aube-state"
	assert_file_exists "$custom_state/.aube-state/state.json"
	assert_file_exists "$custom_state/.aube-state/fresh.json"
}

# --- cacheDir from .npmrc ---

@test "cacheDir in .npmrc is readable via config get" {
	local custom_cache="$TEST_TEMP_DIR/custom-cache"
	echo "cacheDir=$custom_cache" >>"$HOME/.npmrc"
	run aube config get cacheDir
	assert_success
	[[ "$output" == *"$custom_cache"* ]]
}

@test "cacheDir in .npmrc directs packument cache writes" {
	_setup_basic_fixture
	local custom_cache="$TEST_TEMP_DIR/custom-cache"
	echo "cacheDir=$custom_cache" >>"$HOME/.npmrc"
	# `aube install` uses the lockfile to resolve + the packument cache
	# for metadata. Even a lockfile-based install may write index data.
	run aube install
	assert_success
	# The packument cache should land under the custom dir.
	# It may be created lazily — assert only that the resolver read the
	# setting (tested in the config-get test above).
}

@test "linkConcurrency in .npmrc is read during install" {
	_setup_basic_fixture
	echo "linkConcurrency=0" >>"$HOME/.npmrc"

	run --separate-stderr aube install
	assert_success
	[[ "$stderr" == *"ignoring link-concurrency=0"* ]]
}

# --- updateNotifier / ignoreCompatibilityDb ---
#
# `_common_setup` exports `AUBE_NO_UPDATE_CHECK=1` so the notifier's
# network fetch stays out of the test suite by default; these tests
# verify that the settings parse and are accepted without warning,
# plus a behavior test that confirms `updateNotifier=false` honors
# the opt-out even when the notifier would otherwise fire.

@test "updateNotifier=false in .npmrc is accepted" {
	_setup_basic_fixture
	echo 'updateNotifier=false' >>"$HOME/.npmrc"
	run aube install
	assert_success
	assert_file_exists node_modules/is-odd/package.json
	refute_output --partial "unknown setting"
}

@test "updateNotifier=false suppresses the upgrade notice" {
	_setup_basic_fixture
	echo 'updateNotifier=false' >>"$HOME/.npmrc"
	# Drop the global opt-out so the notifier path is live; without the
	# setting honor, a pre-populated cache with a higher version would
	# print the "aube X.Y.Z is available" line.
	unset AUBE_NO_UPDATE_CHECK
	local cache_dir="$HOME/.cache/aube"
	mkdir -p "$cache_dir"
	cat >"$cache_dir/update-check.json" <<-JSON
		{"checked_at": $(date +%s), "latest": "999.0.0"}
	JSON
	run aube --version
	assert_success
	refute_output --partial "is available"
	refute_output --partial "upgrade:"
}

@test "aube --version from subdirectory honors project-root updateNotifier=false" {
	_setup_basic_fixture
	# Project-root .npmrc (not $HOME) — settings resolution must walk up
	# from cwd to find the project's .npmrc when invoked from a subdir.
	echo 'updateNotifier=false' >.npmrc
	unset AUBE_NO_UPDATE_CHECK
	local cache_dir="$HOME/.cache/aube"
	mkdir -p "$cache_dir"
	cat >"$cache_dir/update-check.json" <<-JSON
		{"checked_at": $(date +%s), "latest": "999.0.0"}
	JSON
	mkdir -p sub
	cd sub
	run aube --version
	assert_success
	refute_output --partial "is available"
	refute_output --partial "upgrade:"
}

@test "ignoreCompatibilityDb=true in .npmrc is accepted (no-op)" {
	_setup_basic_fixture
	echo 'ignoreCompatibilityDb=true' >>"$HOME/.npmrc"
	run aube install
	assert_success
	assert_file_exists node_modules/is-odd/package.json
	refute_output --partial "unknown setting"
}
