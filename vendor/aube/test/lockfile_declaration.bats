#!/usr/bin/env bats

# Declaration-aware lockfile-kind resolution (pin-over-inference):
# the package manager `package.json` declares — `packageManager`, or
# `devEngines.packageManager` — outranks both lockfile-filename
# precedence and `defaultLockfileFormat`. Decision-table unit coverage
# lives in aube-lockfile/src/detect.rs; these tests pin the end-to-end
# behavior of the rows a real install can observe.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "declared npm + no lockfile: fresh install writes package-lock.json, not the default format" {
	cat >package.json <<-'EOF'
		{
		  "name": "fresh-with-pin",
		  "version": "1.0.0",
		  "packageManager": "npm@11.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success
	assert_file_exists package-lock.json
	assert [ ! -f aube-lock.yaml ]
	assert_dir_exists node_modules/is-odd
}

@test "declared pnpm keeps pnpm-lock.yaml when a stray package-lock.json is on disk" {
	cat >package.json <<-'EOF'
		{
		  "name": "pin-over-inference",
		  "version": "1.0.0",
		  "packageManager": "pnpm@10.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success
	assert_file_exists pnpm-lock.yaml
	# A contributor runs `npm install` and commits the stray file; the
	# declared PM's lockfile must stay the one aube reads and writes.
	echo '{"lockfileVersion": 3, "packages": {}}' >package-lock.json
	local before
	before="$(cat pnpm-lock.yaml)"
	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists pnpm-lock.yaml
	[ "$(cat pnpm-lock.yaml)" = "$before" ]
}

@test "declared pnpm + only a foreign lockfile errors with the contradiction remedy" {
	cat >package.json <<-'EOF'
		{
		  "name": "contradiction",
		  "version": "1.0.0",
		  "packageManager": "pnpm@10.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	echo '{"lockfileVersion": 3, "packages": {}}' >package-lock.json
	run aube install
	assert_failure
	# miette wraps the message, so assert on fragments that can't span
	# a line break: the stable code, both filenames, and the remedy.
	assert_output --partial 'ERR_AUBE_LOCKFILE_DECLARATION_MISMATCH'
	assert_output --partial 'pnpm-lock.yaml'
	assert_output --partial 'package-lock.json'
	assert_output --partial 'aube import'
	# Nothing written: the contradiction must fail before any resolve.
	assert [ ! -f pnpm-lock.yaml ]
	assert [ ! -f aube-lock.yaml ]
}

@test "undeclared + two package managers' lockfiles errors as ambiguous" {
	cat >package.json <<-'EOF'
		{
		  "name": "ambiguous",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	echo '{"lockfileVersion": 3, "packages": {}}' >package-lock.json
	printf 'lockfileVersion: "9.0"\n' >pnpm-lock.yaml
	run aube install
	assert_failure
	assert_output --partial 'multiple lockfiles'
	assert_output --partial 'pnpm-lock.yaml'
	assert_output --partial 'package-lock.json'
}

@test "devEngines.packageManager pins the fresh format like packageManager does" {
	cat >package.json <<-'EOF'
		{
		  "name": "dev-engines-pin",
		  "version": "1.0.0",
		  "devEngines": { "packageManager": { "name": "npm", "onFail": "warn" } },
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success
	assert_file_exists package-lock.json
	assert [ ! -f aube-lock.yaml ]
}
