#!/usr/bin/env bats
# shellcheck disable=SC2030,SC2031

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_write_pkg() {
	local version="${1:-1.2.3}"
	cat >package.json <<-EOF
		{
		  "name": "version-smoke",
		  "version": "${version}",
		  "scripts": {
		    "build": "echo build"
		  }
		}
	EOF
}

@test "aube version with no args prints current version" {
	_write_pkg 1.2.3
	run aube version
	assert_success
	assert_output "1.2.3"
}

@test "aube version patch bumps patch component" {
	_write_pkg 1.2.3
	run aube version patch --no-git-tag-version
	assert_success
	assert_output "v1.2.4"
	run cat package.json
	assert_output --partial '"version": "1.2.4"'
	# Surrounding fields must be preserved byte-for-byte.
	assert_output --partial '"name": "version-smoke"'
	assert_output --partial '"build": "echo build"'
}

@test "aube version minor bumps minor and resets patch" {
	_write_pkg 1.2.3
	run aube version minor --no-git-tag-version
	assert_success
	assert_output "v1.3.0"
}

@test "aube version major bumps major and resets minor/patch" {
	_write_pkg 1.2.3
	run aube version major --no-git-tag-version
	assert_success
	assert_output "v2.0.0"
}

@test "aube version premajor with preid attaches identifier" {
	_write_pkg 1.2.3
	run aube version premajor --preid rc --no-git-tag-version
	assert_success
	assert_output "v2.0.0-rc.0"
}

@test "aube version prerelease increments numeric tail" {
	_write_pkg 1.2.3-rc.0
	run aube version prerelease --no-git-tag-version
	assert_success
	assert_output "v1.2.3-rc.1"
}

@test "aube version accepts an explicit version" {
	_write_pkg 1.2.3
	run aube version 9.9.9 --no-git-tag-version
	assert_success
	assert_output "v9.9.9"
}

@test "aube version rejects an invalid explicit version" {
	_write_pkg 1.2.3
	run aube version not-a-version --no-git-tag-version
	assert_failure
}

@test "aube version rejects same-version bump without --allow-same-version" {
	_write_pkg 1.2.3
	run aube version 1.2.3 --no-git-tag-version
	assert_failure
	assert_output --partial "already at 1.2.3"
}

@test "aube version --allow-same-version accepts current version" {
	_write_pkg 1.2.3
	run aube version 1.2.3 --allow-same-version --no-git-tag-version
	assert_success
	assert_output "v1.2.3"
}

@test "aube version --json emits a JSON object" {
	_write_pkg 1.2.3
	run aube version patch --json --no-git-tag-version
	assert_success
	assert_output --partial '"version": "1.2.4"'
	assert_output --partial '"previous": "1.2.3"'
}

@test "aube version creates a git commit and tag by default" {
	_write_pkg 1.2.3
	git init -q
	git config user.email "test@example.com"
	git config user.name "Test"
	git add package.json
	git commit -q -m "init"

	run aube version patch
	assert_success
	assert_output "v1.2.4"

	run git tag --list
	assert_output --partial "v1.2.4"

	run git log --format=%s -1
	assert_output "v1.2.4"
}

@test "aube version --allow-same-version in a git repo skips commit + tag" {
	_write_pkg 1.2.3
	git init -q
	git config user.email "test@example.com"
	git config user.name "Test"
	git add package.json
	git commit -q -m "init"

	# Without the skip, `git commit` would error with "nothing to commit".
	run aube version 1.2.3 --allow-same-version
	assert_success
	assert_output "v1.2.3"

	run git log --format=%s
	assert_output "init"
	run git tag --list
	assert_output ""
}

@test "aube version runs preversion, version, postversion in order" {
	cat >package.json <<-'EOF'
		{
		  "name": "version-hooks",
		  "version": "1.0.0",
		  "scripts": {
		    "preversion": "echo preversion >>$HOOK_LOG",
		    "version": "echo version >>$HOOK_LOG",
		    "postversion": "echo postversion >>$HOOK_LOG"
		  }
		}
	EOF

	export HOOK_LOG="$PWD/hooks.log"
	: >"$HOOK_LOG"

	run aube version patch --no-git-tag-version
	assert_success
	assert_output "v1.0.1"

	run cat "$HOOK_LOG"
	assert_success
	assert_line --index 0 "preversion"
	assert_line --index 1 "version"
	assert_line --index 2 "postversion"
}

@test "aube version --ignore-scripts skips lifecycle hooks" {
	cat >package.json <<-'EOF'
		{
		  "name": "version-hooks",
		  "version": "1.0.0",
		  "scripts": {
		    "preversion": "echo preversion >>$HOOK_LOG",
		    "version": "echo version >>$HOOK_LOG",
		    "postversion": "echo postversion >>$HOOK_LOG"
		  }
		}
	EOF

	export HOOK_LOG="$PWD/hooks.log"
	: >"$HOOK_LOG"

	run aube version patch --no-git-tag-version --ignore-scripts
	assert_success

	run cat "$HOOK_LOG"
	assert_success
	assert_output ""
}

@test "aube version preserves manifest edits made by preversion" {
	# Regression: `replace_version` used to operate on the pre-hook
	# snapshot of `package.json`, so any edits `preversion` made to
	# other fields (here: stripping a `draft` flag) were silently
	# overwritten by the atomic write of the new version.
	cat >package.json <<-'EOF'
		{
		  "name": "version-mutates",
		  "version": "1.0.0",
		  "draft": true,
		  "scripts": {
		    "preversion": "node ./strip-draft.mjs"
		  }
		}
	EOF
	cat >strip-draft.mjs <<'NODE'
import fs from 'node:fs';
const m = JSON.parse(fs.readFileSync('package.json', 'utf8'));
delete m.draft;
fs.writeFileSync('package.json', JSON.stringify(m, null, 2));
NODE

	run aube version patch --no-git-tag-version
	assert_success
	assert_output "v1.0.1"

	# Both the preversion edit AND the version bump must survive.
	run cat package.json
	assert_output --partial '"version": "1.0.1"'
	refute_output --partial '"draft"'
}

@test "aube version aborts bump when preversion fails" {
	cat >package.json <<-'EOF'
		{
		  "name": "version-hooks",
		  "version": "1.0.0",
		  "scripts": {
		    "preversion": "exit 7"
		  }
		}
	EOF

	run aube version patch --no-git-tag-version
	assert_failure

	# Manifest should be untouched since preversion fires BEFORE the edit.
	run cat package.json
	assert_output --partial '"version": "1.0.0"'
}
