#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install --resolution-mode=time-based succeeds and writes time: block" {
	_setup_basic_fixture
	# Force a re-resolve so the resolver actually runs against the
	# fixture registry (the shipped lockfile has no time: block, and
	# reuse would otherwise short-circuit the new code path).
	rm aube-lock.yaml
	run aube install --resolution-mode=time-based
	assert_success
	assert_file_exists aube-lock.yaml
	run grep -E '^time:' aube-lock.yaml
	assert_success
	# The fixture packuments are committed with real ISO-8601 publish
	# timestamps, so the lockfile should end up with at least one
	# `name@version: 20..-..-..T..:..Z` entry under `time:`. serde_yaml
	# may or may not wrap the key in quotes depending on whether it
	# contains a `@`, so match both shapes.
	run grep -E "[a-z0-9@/._-]+@[0-9]+\\.[0-9]+\\.[0-9]+.*: ['\"]?[0-9]{4}-" aube-lock.yaml
	assert_success
}

@test "aube install --resolution-mode=highest writes no time: block (pnpm parity)" {
	_setup_basic_fixture
	rm aube-lock.yaml
	run aube install --resolution-mode=highest
	assert_success
	assert_file_exists aube-lock.yaml
	# pnpm aggregates publish times into the lockfile *only* under
	# resolution-mode=time-based: resolveDependencies.ts populates its
	# `time` map solely inside the `if (ctx.resolutionMode ===
	# 'time-based')` branch, and updateLockfile then guards
	# `newLockfile.time = …` on that map. Highest mode must therefore
	# stay time:-free even though the fixture Verdaccio ships `time` in
	# its corgi responses — matching pnpm. (Regression: aube used to
	# round-trip the field opportunistically in every mode.)
	run grep -E '^time:' aube-lock.yaml
	assert_failure
}

@test "aube install --resolution-mode rejects unknown values via .npmrc fallback" {
	_setup_basic_fixture
	rm aube-lock.yaml
	# Unknown CLI value silently falls back to the .npmrc/default
	# (highest) path; the install should still succeed with classic
	# highest resolution, which — like pnpm — writes no time: block.
	run aube install --resolution-mode=bogus
	assert_success
	run grep -E '^time:' aube-lock.yaml
	assert_failure
}
