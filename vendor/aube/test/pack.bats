#!/usr/bin/env bats
# shellcheck disable=SC2030,SC2031

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_write_pkg_with_files() {
	cat >package.json <<-'EOF'
		{
		  "name": "pack-smoke",
		  "version": "1.2.3",
		  "main": "index.js",
		  "files": ["index.js", "lib"]
		}
	EOF
	cat >index.js <<-'EOF'
		module.exports = 42
	EOF
	mkdir -p lib
	cat >lib/helper.js <<-'EOF'
		module.exports = { helper: true }
	EOF
	# A file that should NOT land in the tarball because it isn't in `files`.
	cat >NOPE.md <<-'EOF'
		excluded
	EOF
	# A README that should be pulled in via the always-include rule even
	# though it's not listed in `files`.
	cat >README.md <<-'EOF'
		# pack-smoke
	EOF
}

@test "aube pack builds a tarball from the files field" {
	_write_pkg_with_files

	run aube pack
	assert_success
	assert_output --partial "pack-smoke@1.2.3"
	assert_output --partial "package.json"
	assert_output --partial "index.js"
	assert_output --partial "lib/helper.js"
	assert_output --partial "README.md"
	refute_output --partial "NOPE.md"

	assert_file_exists pack-smoke-1.2.3.tgz

	# Tarball entries should be rooted at `package/`.
	run tar -tzf pack-smoke-1.2.3.tgz
	assert_success
	assert_line "package/package.json"
	assert_line "package/index.js"
	assert_line "package/lib/helper.js"
	assert_line "package/README.md"
	refute_line "package/NOPE.md"
}

@test "aube pack --json emits a machine-readable report" {
	_write_pkg_with_files

	run aube pack --json
	assert_success
	assert_output --partial '"name": "pack-smoke"'
	assert_output --partial '"version": "1.2.3"'
	assert_output --partial '"filename": "pack-smoke-1.2.3.tgz"'
	assert_output --partial '"path": "index.js"'
}

@test "aube pack --dry-run prints contents but writes no tarball" {
	_write_pkg_with_files

	run aube pack --dry-run
	assert_success
	assert_output --partial "pack-smoke@1.2.3"
	assert_output --partial "index.js"

	[ ! -f pack-smoke-1.2.3.tgz ]
}

@test "aube pack sanitizes scoped package names" {
	cat >package.json <<-'EOF'
		{
		  "name": "@aube-fixture/pack",
		  "version": "0.1.0",
		  "files": ["index.js"]
		}
	EOF
	cat >index.js <<-'EOF'
		module.exports = 0
	EOF

	run aube pack
	assert_success
	assert_file_exists aube-fixture-pack-0.1.0.tgz
}

@test "aube pack --pack-destination writes to a specific directory" {
	_write_pkg_with_files
	mkdir dist

	run aube pack --pack-destination dist
	assert_success
	assert_file_exists dist/pack-smoke-1.2.3.tgz
	[ ! -f pack-smoke-1.2.3.tgz ]
}

@test "aube pack --pack-destination from a subdirectory is relative to that subdirectory" {
	_write_pkg_with_files
	mkdir -p docs
	cd docs

	run aube pack --pack-destination out
	assert_success
	assert_file_exists out/pack-smoke-1.2.3.tgz
	assert_not_exists "$TEST_TEMP_DIR/out/pack-smoke-1.2.3.tgz"

	run tar -tzf out/pack-smoke-1.2.3.tgz
	assert_success
	assert_line "package/package.json"
	assert_line "package/index.js"
}

@test "aube pack runs prepack, prepare, postpack in order" {
	cat >package.json <<-'EOF'
		{
		  "name": "pack-hooks",
		  "version": "1.0.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "scripts": {
		    "prepack": "echo prepack >>$HOOK_LOG",
		    "prepare": "echo prepare >>$HOOK_LOG",
		    "postpack": "echo postpack >>$HOOK_LOG"
		  }
		}
	EOF
	cat >index.js <<-'EOF'
		module.exports = 1
	EOF

	export HOOK_LOG="$PWD/hooks.log"
	: >"$HOOK_LOG"

	run aube pack
	assert_success

	run cat "$HOOK_LOG"
	assert_success
	assert_line --index 0 "prepack"
	assert_line --index 1 "prepare"
	assert_line --index 2 "postpack"
}

@test "aube pack --ignore-scripts skips lifecycle hooks" {
	cat >package.json <<-'EOF'
		{
		  "name": "pack-hooks",
		  "version": "1.0.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "scripts": {
		    "prepack": "echo prepack >>$HOOK_LOG",
		    "prepare": "echo prepare >>$HOOK_LOG",
		    "postpack": "echo postpack >>$HOOK_LOG"
		  }
		}
	EOF
	cat >index.js <<-'EOF'
		module.exports = 1
	EOF

	export HOOK_LOG="$PWD/hooks.log"
	: >"$HOOK_LOG"

	run aube pack --ignore-scripts
	assert_success

	# Log should still exist but be empty — no hook fired.
	run cat "$HOOK_LOG"
	assert_success
	assert_output ""
}

@test "aube pack propagates a failing prepack" {
	cat >package.json <<-'EOF'
		{
		  "name": "pack-hooks",
		  "version": "1.0.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "scripts": {
		    "prepack": "exit 3"
		  }
		}
	EOF
	cat >index.js <<-'EOF'
		module.exports = 1
	EOF

	run aube pack
	assert_failure
	[ ! -f pack-hooks-1.0.0.tgz ]
}
