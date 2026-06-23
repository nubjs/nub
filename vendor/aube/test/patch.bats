#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Round-trip the whole patch workflow against a fixture package: install,
# patch, modify a file, commit, verify the linked tree picked up the
# change, then patch-remove and verify the original bytes return.
@test "aube patch round-trips through patch-commit and patch-remove" {
	cat >package.json <<'EOF'
{
  "name": "patch-test",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
EOF
	run aube install
	assert_success

	# Sanity-check the unpatched file content. is-odd@3.0.1 ships an
	# `index.js` we can match a known line from.
	original_line="$(grep "module.exports" node_modules/is-odd/index.js)"
	[ -n "$original_line" ]

	# Extract the package.
	run aube patch is-odd@3.0.1
	assert_success
	assert_output --partial "You can now edit the following folder:"
	edit_dir="$(echo "$output" | grep -oE '/[^ ]*/user' | head -1)"
	[ -d "$edit_dir" ]

	# Modify a file in the edit dir — append a sentinel comment so the
	# diff is small but unambiguous.
	echo "// patched-by-aube-test" >>"$edit_dir/index.js"

	run aube patch-commit "$edit_dir"
	assert_success
	assert [ -f patches/is-odd@3.0.1.patch ]
	# No `pnpm` namespace in the test's package.json, so the
	# unified writer rule lands the entry under `aube.patchedDependencies`.
	run node -e 'console.log(require("./package.json").aube.patchedDependencies["is-odd@3.0.1"])'
	assert_output "patches/is-odd@3.0.1.patch"

	# The linked package should now contain the sentinel.
	run grep -q "patched-by-aube-test" node_modules/is-odd/index.js
	assert_success

	# Removing the patch should drop the file, the manifest entry, and
	# the sentinel from the linked tree.
	run aube patch-remove is-odd@3.0.1
	assert_success
	assert [ ! -f patches/is-odd@3.0.1.patch ]
	run node -e 'const p = require("./package.json"); console.log(p.aube ? Object.keys(p.aube.patchedDependencies||{}).length : 0)'
	assert_output "0"
	run grep -q "patched-by-aube-test" node_modules/is-odd/index.js
	assert_failure
}

@test "aube patch rejects bare names" {
	cat >package.json <<'EOF'
{ "name": "p", "version": "1.0.0" }
EOF
	run aube patch is-odd
	assert_failure
	assert_output --partial "requires"
}

@test "aube patch-commit works from workspace package with --workspace-root" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	mkdir -p packages/app
	cat >packages/app/package.json <<'EOF'
{
  "name": "patch-workspace-package",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
EOF

	(
		cd packages/app
		run aube install
		assert_success

		run aube patch is-odd@3.0.1
		assert_success
		edit_dir="$(echo "$output" | grep -oE '/[^ ]*/user' | head -1)"
		echo "// patched-from-workspace-package" >>"$edit_dir/index.js"

		run aube --workspace-root patch-commit "$edit_dir"
		assert_success
		run grep -q "patched-from-workspace-package" node_modules/is-odd/index.js
		assert_success
	)
}

@test "aube patch errors when the package is not installed" {
	cat >package.json <<'EOF'
{ "name": "p", "version": "1.0.0" }
EOF
	run aube patch is-odd@3.0.1
	assert_failure
	assert_output --partial "is not installed"
}
