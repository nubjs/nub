#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# `gitBranchLockfile: true` makes aube write a per-branch lockfile so
# long-lived branches don't fight over `aube-lock.yaml`. Forward slashes
# in branch names are encoded as `!` to keep the filename portable.

@test "gitBranchLockfile writes aube-lock.<branch>.yaml" {
	git init -q
	git checkout -q -b feature/x
	cat >package.json <<-'EOF'
		{
		  "name": "test-gbl",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		gitBranchLockfile: true
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.feature!x.yaml
	run test -e aube-lock.yaml
	assert_failure
}

@test "gitBranchLockfile=false (default) writes aube-lock.yaml" {
	git init -q
	git checkout -q -b feature/x
	cat >package.json <<-'EOF'
		{
		  "name": "test-gbl",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.yaml
	run test -e 'aube-lock.feature!x.yaml'
	assert_failure
}

@test "gitBranchLockfile reads existing aube-lock.yaml when no branch file" {
	# Turn the setting on mid-project: the existing base lockfile should
	# still satisfy a frozen install on the new branch.
	git init -q
	git checkout -q -b main
	cat >package.json <<-'EOF'
		{
		  "name": "test-gbl",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.yaml

	git checkout -q -b feature/x
	cat >pnpm-workspace.yaml <<-'EOF'
		gitBranchLockfile: true
	EOF
	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success

	# Regression: state.rs used to hash the (non-existent) branch lockfile
	# path here, leaving the saved hash empty. The next ensure_installed
	# call would then re-trigger an install on every aube run/exec.
	# `aube run --help` calls ensure_installed; with the bug, this prints
	# a "no lockfile found" / re-install message even though node_modules
	# is already up to date. We assert the install state is stable by
	# running `aube install` a second time and checking it short-circuits.
	run aube install
	assert_success
	refute_output --partial "no lockfile found"
}

@test "gitBranchLockfile missing lockfile restore respects current branch name" {
	git init -q
	git config user.email "t@t"
	git config user.name "t"
	git checkout -q -b main
	cat >package.json <<-'EOF'
		{
		  "name": "test-gbl-restore",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		gitBranchLockfile: true
	EOF
	git add package.json pnpm-workspace.yaml
	git commit -q -m "init"

	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.main.yaml
	assert_file_exists node_modules/.aube-state/lockfile

	git checkout -q -b dev
	rm aube-lock.main.yaml
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.dev.yaml
}

# ===================================================================
# mergeGitBranchLockfilesBranchPattern / --merge-git-branch-lockfiles
# ===================================================================
#
# Setup pattern: create a base `aube-lock.yaml` on `main`, switch to
# feature branches with `gitBranchLockfile: true` and generate per-branch
# lockfiles, then land on a collapse branch and merge.

@test "--merge-git-branch-lockfiles collapses branch lockfiles into aube-lock.yaml" {
	git init -q
	git config user.email "t@t"
	git config user.name "t"
	git checkout -q -b main
	cat >package.json <<-'EOF'
		{
		  "name": "test-merge",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		gitBranchLockfile: true
	EOF
	# Commit so branches are real (not unborn) and we can switch between them.
	git add package.json pnpm-workspace.yaml
	git commit -q -m "init"

	# Base install on main produces aube-lock.main.yaml.
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.main.yaml

	# Simulate a feature branch lockfile by copying main's.
	git checkout -q -b feature/a
	cp aube-lock.main.yaml aube-lock.feature!a.yaml

	# Back to main, run the merge flag.
	git checkout -q main
	run aube install --merge-git-branch-lockfiles --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.yaml
	# The `feature/a` branch file is deleted after a successful merge.
	run test -e 'aube-lock.feature!a.yaml'
	assert_failure
	# Note: on main with gitBranchLockfile=true the install re-writes
	# `aube-lock.main.yaml` after merging, which is expected — the
	# merge is a one-shot consolidation, not a permanent switch.
}

@test "mergeGitBranchLockfilesBranchPattern auto-triggers merge on matching branch" {
	git init -q
	git config user.email "t@t"
	git config user.name "t"
	git checkout -q -b main
	cat >package.json <<-'EOF'
		{
		  "name": "test-merge-auto",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		gitBranchLockfile: true
		mergeGitBranchLockfilesBranchPattern:
		  - main
	EOF
	# Install on main with the setting on should trigger the merge
	# (no-op since there are no branch files yet, but should not error).
	run aube install --no-frozen-lockfile
	assert_success
	# On `main` with gitBranchLockfile + the pattern matching, the
	# main-branch lockfile is written as `aube-lock.main.yaml`. The
	# merge runs before install but finds only that file (which it
	# excludes? no — it's a branch file). Subsequent install on `main`
	# should merge that into aube-lock.yaml.
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists aube-lock.yaml
}

@test "mergeGitBranchLockfilesBranchPattern does NOT trigger merge on non-matching branch" {
	git init -q
	git config user.email "t@t"
	git config user.name "t"
	git checkout -q -b feature/x
	cat >package.json <<-'EOF'
		{
		  "name": "test-merge-nomatch",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		gitBranchLockfile: true
		mergeGitBranchLockfilesBranchPattern:
		  - main
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	# Branch file is written normally; base file is NOT created because
	# no merge happened.
	assert_file_exists 'aube-lock.feature!x.yaml'
	run test -e aube-lock.yaml
	assert_failure
}

@test "--merge-git-branch-lockfiles is a no-op when no branch lockfiles exist" {
	git init -q
	git config user.email "t@t"
	git config user.name "t"
	git checkout -q -b main
	cat >package.json <<-'EOF'
		{
		  "name": "test-merge-empty",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install --merge-git-branch-lockfiles --no-frozen-lockfile
	assert_success
	# No branch files existed; install proceeds and writes aube-lock.yaml.
	assert_file_exists aube-lock.yaml
}

@test "mergeGitBranchLockfilesBranchPattern honors ! negation" {
	git init -q
	git config user.email "t@t"
	git config user.name "t"
	git checkout -q -b 'release/legacy-v0'
	cat >package.json <<-'EOF'
		{
		  "name": "test-merge-neg",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		gitBranchLockfile: true
		mergeGitBranchLockfilesBranchPattern:
		  - "release/*"
		  - "!release/legacy-*"
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	# `release/legacy-v0` matches the positive pattern but is excluded
	# by the `!release/legacy-*` negation, so no merge runs.
	assert_file_exists 'aube-lock.release!legacy-v0.yaml'
	run test -e aube-lock.yaml
	assert_failure
}
