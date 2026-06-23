#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
	# Route global installs into the per-test temp dir so nothing escapes
	# the sandbox. `bin_dir` = AUBE_HOME, `pkg_dir` = AUBE_HOME/global-aube.
	# aube prints AUBE_HOME as-is (no canonicalize), so compare against the
	# env var verbatim, not `pwd -P`.
	export AUBE_HOME="$TEST_TEMP_DIR/aube-home"
	mkdir -p "$AUBE_HOME"
}

teardown() {
	_common_teardown
}

@test "aube bin -g prints the global bin directory" {
	run aube bin -g
	assert_success
	assert_output "$AUBE_HOME"
}

@test "aube root -g prints the global package directory" {
	run aube root -g
	assert_success
	assert_output "$AUBE_HOME/global-aube"
}

@test "aube bin -g honors PNPM_HOME when AUBE_HOME is unset" {
	unset AUBE_HOME
	PNPM_HOME="$TEST_TEMP_DIR/pnpm-home" run aube bin -g
	assert_success
	assert_output "$TEST_TEMP_DIR/pnpm-home"
}

@test "aube list -g reports nothing on an empty global dir" {
	run aube list -g
	assert_success
	assert_output --partial "no global packages installed"
}

@test "aube add -g installs a package and links its bin" {
	run aube add -g semver@7.7.4
	assert_success

	# bin symlink lands in $AUBE_HOME
	assert_file_exists "$AUBE_HOME/semver"

	# list -g picks it up
	run aube list -g
	assert_success
	assert_output --partial "semver 7.7.4"

	# parseable output is one tab-separated line per package
	run aube list -g --parseable
	assert_success
	assert_output --partial "	semver	7.7.4"

	# json output includes name/version
	run aube list -g --json
	assert_success
	assert_output --partial '"name": "semver"'
	assert_output --partial '"version": "7.7.4"'
}

@test "aube update -g updates global packages outside a project" {
	run aube add -g is-positive@1.0.0
	assert_success

	mkdir -p "$TEST_TEMP_DIR/no-project"
	cd "$TEST_TEMP_DIR/no-project"
	assert_file_not_exists package.json

	run aube update -g
	assert_success
	refute_output --partial "no package.json found"
	assert_output --partial "is-positive: 1.0.0 ->"

	run aube list -g
	assert_success
	assert_output --partial "is-positive "
	refute_output --partial "is-positive 1.0.0"
}

@test "aube update -g fails when any named package is missing" {
	run aube add -g is-positive@1.0.0
	assert_success

	run aube update -g is-positive missing-global-package
	assert_failure
	assert_output --partial "not globally installed: missing-global-package"
}

@test "aube add -g creates a hash pointer in the pkg dir" {
	run aube add -g semver@7.7.4
	assert_success

	# At least one symlink entry (the hash) should exist in the pkg dir
	pkg_dir="$AUBE_HOME/global-aube"
	run bash -c "find '$pkg_dir' -maxdepth 1 -type l | wc -l | tr -d ' '"
	assert_success
	assert_output "1"
}

@test "aube add -g twice replaces the prior install" {
	run aube add -g semver@7.7.4
	assert_success
	run aube add -g semver@7.7.4
	assert_success

	# Only one install dir + one hash pointer should remain.
	pkg_dir="$AUBE_HOME/global-aube"
	run bash -c "find '$pkg_dir' -maxdepth 1 -type l | wc -l | tr -d ' '"
	assert_output "1"
	run bash -c "find '$pkg_dir' -maxdepth 1 -type d | tail -n +2 | wc -l | tr -d ' '"
	assert_output "1"
}

@test "aube remove -g deletes the install and unlinks its bin" {
	run aube add -g semver@7.7.4
	assert_success
	assert_file_exists "$AUBE_HOME/semver"

	run aube remove -g semver
	assert_success
	assert_file_not_exists "$AUBE_HOME/semver"

	run aube list -g
	assert_success
	assert_output --partial "no global packages installed"
}

@test "aube remove -g unlinks a preferSymlinkedExecutables=false shell shim" {
	# With the setting off, the global `.bin/<name>` is a regular-file
	# shell shim rather than a symlink. `unlink_bins` still has to
	# recognize it as ours and remove it on `aube remove -g`.
	cat >"$HOME/.npmrc" <<-EOF
		registry=${AUBE_TEST_REGISTRY}
		preferSymlinkedExecutables=false
	EOF

	run aube add -g semver@7.7.4
	assert_success
	assert_file_exists "$AUBE_HOME/semver"
	# Regular file, not a symlink — the whole point of this scenario.
	[ ! -L "$AUBE_HOME/semver" ]

	run aube remove -g semver
	assert_success
	assert_file_not_exists "$AUBE_HOME/semver"
}

@test "aube list -g applies the positional name filter" {
	run aube add -g semver@7.7.4
	assert_success

	# Non-matching prefix hides the entry
	run aube list -g nothing
	assert_success
	refute_output --partial "semver"

	# Matching prefix keeps it
	run aube list -g sem
	assert_success
	assert_output --partial "semver 7.7.4"
}

@test "aube ignored-builds -g lists skipped global dependency builds" {
	run aube add -g aube-test-builds-marker@1.0.0
	assert_success
	assert_output --partial "ignored build scripts"

	run aube ignored-builds -g
	assert_success
	assert_output --partial "The following global builds were ignored"
	assert_output --partial "aube-test-builds-marker@1.0.0"
}

@test "aube approve-builds -g --all writes approvals into global installs" {
	run aube add -g aube-test-builds-marker@1.0.0
	assert_success

	pkg_dir="$AUBE_HOME/global-aube"
	install_dir="$(find "$pkg_dir" -mindepth 1 -maxdepth 1 -type d -print -quit)"
	assert_file_not_exists "$install_dir/aube-builds-marker.txt"

	run aube approve-builds -g --all
	assert_success
	assert_output --partial "aube-test-builds-marker"
	assert_output --partial "global install"

	# Global installs use the same pnpm v11 review map as projects.
	# When no workspace yaml exists, the entry lands in package.json
	# under the `aube` namespace (the writer prefers `aube.*` until a
	# `pnpm` namespace is already present).
	assert_file_not_exists "$install_dir/aube-workspace.yaml"
	run grep -q '"aube"' "$install_dir/package.json"
	assert_success
	run grep -q '"allowBuilds"' "$install_dir/package.json"
	assert_success
	run grep -q '"aube-test-builds-marker": true' "$install_dir/package.json"
	assert_success

	run aube -C "$install_dir" rebuild
	assert_success
	assert_file_exists "$install_dir/aube-builds-marker.txt"
}

@test "aube add -g --allow-build=<pkg> pre-approves a global dep's build scripts" {
	# Regression for Discussion #617: outer `--allow-build` was dropped
	# at the `run_global_inner` boundary (synthetic AddArgs hardcoded
	# `allow_build: Vec::new()`), so under `strictDepBuilds=true` the
	# install errored with "must be reviewed before install" even when
	# the user explicitly approved the dep on the CLI.
	AUBE_STRICT_DEP_BUILDS=true run aube add -g \
		--allow-build=aube-test-builds-marker \
		aube-test-builds-marker@1.0.0
	assert_success
	refute_output --partial "must be reviewed before install"
	refute_output --partial "ignored build scripts"

	pkg_dir="$AUBE_HOME/global-aube"
	install_dir="$(find "$pkg_dir" -mindepth 1 -maxdepth 1 -type d -print -quit)"
	# Build actually ran — the marker dep's postinstall writes the file.
	assert_file_exists "$install_dir/aube-builds-marker.txt"
	# Approval landed in the throwaway install dir's package.json under
	# the `aube` namespace (no workspace yaml exists there).
	run grep -q '"aube-test-builds-marker": true' "$install_dir/package.json"
	assert_success
}

@test "aube add -g --deny-build=<pkg> marks a global dep's build scripts reviewed" {
	AUBE_STRICT_DEP_BUILDS=true run aube add -g \
		--deny-build=aube-test-builds-marker \
		aube-test-builds-marker@1.0.0
	assert_success
	refute_output --partial "must be reviewed before install"
	refute_output --partial "ignored build scripts"

	pkg_dir="$AUBE_HOME/global-aube"
	install_dir="$(find "$pkg_dir" -mindepth 1 -maxdepth 1 -type d -print -quit)"
	# Denied dep stayed skipped, but strictDepBuilds accepted the
	# explicit review decision.
	assert_file_not_exists "$install_dir/aube-builds-marker.txt"
	run grep -q '"aube-test-builds-marker": false' "$install_dir/package.json"
	assert_success
}

@test "aube remove -g on a missing package errors" {
	run aube remove -g nonexistent-pkg
	assert_failure
	assert_output --partial "no matching global packages were removed"
}
