#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install honors minimumReleaseAge=0 (feature disabled via .npmrc)" {
	_setup_basic_fixture
	rm aube-lock.yaml
	echo "minimumReleaseAge=0" >.npmrc
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
}

@test "aube install honors minimumReleaseAge=0 in pnpm-workspace.yaml" {
	# Regression: a previous draft inverted the precedence and let
	# .npmrc shadow workspace.yaml. With workspace.yaml disabling the
	# gate and a stale .npmrc carrying an impossible cutoff, install
	# must succeed (workspace wins).
	_setup_basic_fixture
	rm aube-lock.yaml
	cat >pnpm-workspace.yaml <<EOF
packages:
  - "."
minimumReleaseAge: 0
EOF
	cat >.npmrc <<EOF
minimumReleaseAge=999999999
minimumReleaseAgeStrict=true
EOF
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
}

@test "aube install with huge minimumReleaseAge falls back to lowest satisfying" {
	# pnpm v11's lenient default: when every satisfying version is
	# younger than the cutoff, fall back to the lowest version that
	# satisfies the range, ignoring the cutoff for that pick. The
	# fixture packuments are years old, so a 1-billion-minute cutoff
	# (~1900 years) excludes every candidate and forces the fallback.
	_setup_basic_fixture
	rm aube-lock.yaml
	echo "minimumReleaseAge=999999999" >.npmrc
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
}

@test "aube install with minimumReleaseAgeStrict=true and impossible cutoff fails" {
	_setup_basic_fixture
	rm aube-lock.yaml
	cat >.npmrc <<EOF
minimumReleaseAge=999999999
minimumReleaseAgeStrict=true
EOF
	run aube install
	assert_failure
}

@test "minimumReleaseAge + trustPolicy do not add a time: block under default resolution (pnpm parity)" {
	# Regression for the reported pnpm <-> aube incoherence: with
	# `minimumReleaseAge` (and `trustPolicy: no-downgrade`) set but the
	# default `resolution-mode=highest`, pnpm writes no top-level `time:`
	# block — it enforces both policies from a separate on-disk metadata
	# cache, persisting `time:` only under `resolution-mode=time-based`.
	# aube used to leak a `time:` block here because `should_record_times`
	# also keyed off these two policies. The fixture packages are years
	# old, so a one-week cutoff resolves cleanly.
	_setup_basic_fixture
	rm aube-lock.yaml
	cat >pnpm-workspace.yaml <<EOF
packages:
  - "."
minimumReleaseAge: 10080
trustPolicy: no-downgrade
EOF
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
	run grep -E '^time:' aube-lock.yaml
	assert_failure
}

@test "minimumReleaseAgeExclude lets named packages bypass the cutoff" {
	# With strict mode + an impossible cutoff, the install would fail
	# unless every range in the dep tree is excluded. Excluding the
	# fixture's two roots is enough to make resolution succeed.
	_setup_basic_fixture
	rm aube-lock.yaml
	cat >pnpm-workspace.yaml <<EOF
packages:
  - "."
minimumReleaseAge: 999999999
minimumReleaseAgeStrict: true
minimumReleaseAgeExclude:
  - is-odd
  - is-even
  - is-number
  - is-buffer
  - kind-of
EOF
	run aube install
	assert_success
	assert_file_exists aube-lock.yaml
}
