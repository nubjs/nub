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

@test "patch-commit extends an existing patch at its declared path" {
	cat >package.json <<'EOF'
{
  "name": "patch-existing-test",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - .
EOF

	run aube install --ignore-scripts
	assert_success

	run aube patch is-odd@3.0.1
	assert_success
	edit_dir="$(echo "$output" | grep -oE '/[^ ]*/user' | head -1)"
	echo "// existing-patch-marker" >>"$edit_dir/index.js"

	run aube patch-commit --patches-dir patches/custom "$edit_dir"
	assert_success
	generated_patch="patches/custom/is-odd@3.0.1.patch"
	declared_patch="patches/is-odd__3.0.1.patch"
	mv "$generated_patch" "$declared_patch"
	sed -i.bak "s#$generated_patch#$declared_patch#" pnpm-workspace.yaml
	rm pnpm-workspace.yaml.bak
	run aube install --ignore-scripts
	assert_success

	run aube patch is-odd@3.0.1
	assert_success
	edit_dir="$(echo "$output" | grep -oE '/[^ ]*/user' | head -1)"
	run grep -q "existing-patch-marker" "$edit_dir/index.js"
	assert_success
	echo "// second-patch-marker" >>"$edit_dir/index.js"
	retry_parent="$BATS_TEST_TMPDIR/retry-edit"
	cp -R "$(dirname "$edit_dir")" "$retry_parent"

	run aube patch-commit "$edit_dir"
	assert_success
	run aube patch-commit "$retry_parent/user"
	assert_success
	assert [ -f "$declared_patch" ]
	assert [ ! -f "patches/is-odd@3.0.1.patch" ]
	run grep -q '^+// existing-patch-marker' "$declared_patch"
	assert_success
	run grep -c '^+// second-patch-marker' "$declared_patch"
	assert_output "1"
	run grep -Fq "$declared_patch" pnpm-workspace.yaml
	assert_success

	rm -rf node_modules
	run aube install --ignore-scripts
	assert_success
	run grep -q "existing-patch-marker" node_modules/is-odd/index.js
	assert_success
	run grep -q "second-patch-marker" node_modules/is-odd/index.js
	assert_success
}

@test "pnpm patch hash drift fails frozen install and refreshes plain install" {
	cat >package.json <<'EOF'
{
  "name": "patch-hash-drift-test",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - .
patchedDependencies:
  is-odd@3.0.1: patches/is-odd@3.0.1.patch
EOF
	mkdir patches
	cat >patches/is-odd@3.0.1.patch <<'EOF'
diff --git a/index.js b/index.js
index 79d1f22a8e7a27efb8841bb83cb682ea1ff3a59c..1e33b4cf949b73bde8861ad65de71b4e46360259 100644
--- a/index.js
+++ b/index.js
@@ -24,1 +24,2 @@ module.exports = function isOdd(value) {
 };
+module.exports.patched = 'v1';
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'
importers:
  .: {}
EOF

	run aube install --no-frozen-lockfile --ignore-scripts
	assert_success
	old_hash="$(awk '$1 == "is-odd@3.0.1:" && NF == 2 { print $2; exit }' pnpm-lock.yaml)"
	assert_equal "${#old_hash}" 64

	sed -i.bak "s/patched = 'v1'/patched = 'v2'/" patches/is-odd@3.0.1.patch
	rm patches/is-odd@3.0.1.patch.bak

	run aube install --frozen-lockfile --ignore-scripts
	assert_failure
	assert_output --partial "ERR_AUBE_LOCKFILE_CONFIG_MISMATCH"

	run aube install --ignore-scripts
	assert_success
	new_hash="$(awk '$1 == "is-odd@3.0.1:" && NF == 2 { print $2; exit }' pnpm-lock.yaml)"
	assert_equal "${#new_hash}" 64
	[ "$new_hash" != "$old_hash" ]
	run grep -Fq "version: 3.0.1(patch_hash=$new_hash)" pnpm-lock.yaml
	assert_success
	run node -e 'if (require("is-odd").patched !== "v2") process.exit(1)'
	assert_success
}

# A patch declared against a package's registry name must apply when the
# package is installed under an npm alias. Regression for discussion
# #1082: the patch map is keyed by the registry selector
# (`is-odd@3.0.1`) while the aliased entry's `spec_key()` is
# `odd-alias@3.0.1`, so the apply site missed the patch even though the
# lockfile recorded it.
@test "patch keyed by registry name applies to an npm-aliased install" {
	cat >package.json <<'EOF'
{
  "name": "aliased-patch-test",
  "version": "1.0.0",
  "dependencies": { "odd-alias": "npm:is-odd@3.0.1" },
  "pnpm": {
    "patchedDependencies": { "is-odd@3.0.1": "patches/is-odd@3.0.1.patch" }
  }
}
EOF
	mkdir patches
	cat >patches/is-odd@3.0.1.patch <<'EOF'
diff --git a/index.js b/index.js
index 79d1f22a8e7a27efb8841bb83cb682ea1ff3a59c..1e33b4cf949b73bde8861ad65de71b4e46360259 100644
--- a/index.js
+++ b/index.js
@@ -24,1 +24,2 @@ module.exports = function isOdd(value) {
 };
+module.exports.patched = 'v1';
EOF

	run aube install --ignore-scripts
	assert_success

	# The aliased folder's bytes must carry the patch.
	run grep -q "module.exports.patched = 'v1';" node_modules/odd-alias/index.js
	assert_success

	# Requiring through the alias should see the patched export.
	run node -e 'if (require("odd-alias").patched !== "v1") process.exit(1)'
	assert_success
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

@test "non-frozen install ignores stale pnpm lockfile patch entries" {
	cat >package.json <<'EOF'
{
  "name": "stale-patch-test",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - .
patchedDependencies:
  is-odd@3.0.1: patches/is-odd@3.0.1.patch
EOF
	mkdir patches
	cat >patches/is-odd@3.0.1.patch <<'EOF'
diff --git a/index.js b/index.js
index 79d1f22a8e7a27efb8841bb83cb682ea1ff3a59c..1e33b4cf949b73bde8861ad65de71b4e46360259 100644
--- a/index.js
+++ b/index.js
@@ -24,1 +24,2 @@ module.exports = function isOdd(value) {
 };
+module.exports.patched = 'v1';
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'
patchedDependencies:
  is-odd@3.0.0:
    hash: stale
    path: patches/is-odd@3.0.0.patch
importers:
  .:
    dependencies:
      is-odd:
        specifier: 3.0.0
        version: 3.0.0
packages:
  is-odd@3.0.0:
    resolution: {integrity: sha512-stale}
snapshots:
  is-odd@3.0.0: {}
EOF

	run aube install --no-frozen-lockfile --ignore-scripts
	assert_success
	run node -e 'const odd = require("is-odd"); if (!odd.patched) process.exit(1)'
	assert_success
	run grep -q 'is-odd@3.0.0' pnpm-lock.yaml
	assert_failure
	run grep -q 'is-odd@3.0.1:' pnpm-lock.yaml
	assert_success
}

@test "non-frozen install ignores stale pnpm hash-only patch entries" {
	cat >package.json <<'EOF'
{
  "name": "stale-hash-patch-test",
  "version": "1.0.0",
  "dependencies": { "is-odd": "3.0.1" }
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - .
patchedDependencies:
  is-odd@3.0.1: patches/is-odd@3.0.1.patch
EOF
	mkdir patches
	cat >patches/is-odd@3.0.1.patch <<'EOF'
diff --git a/index.js b/index.js
index 79d1f22a8e7a27efb8841bb83cb682ea1ff3a59c..1e33b4cf949b73bde8861ad65de71b4e46360259 100644
--- a/index.js
+++ b/index.js
@@ -24,1 +24,2 @@ module.exports = function isOdd(value) {
 };
+module.exports.patched = 'v1';
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'
patchedDependencies:
  is-odd@3.0.0: 02efb892c0aa62e77ab535074021159d4eb5764f187cecb6b759227dcc9ebfec
importers:
  .:
    dependencies:
      is-odd:
        specifier: 3.0.0
        version: 3.0.0
packages:
  is-odd@3.0.0:
    resolution: {integrity: sha512-stale}
snapshots:
  is-odd@3.0.0: {}
EOF

	run aube install --no-frozen-lockfile --ignore-scripts
	assert_success
	run node -e 'const odd = require("is-odd"); if (!odd.patched) process.exit(1)'
	assert_success
	run grep -q 'is-odd@3.0.0' pnpm-lock.yaml
	assert_failure
	run grep -q 'is-odd@3.0.1:' pnpm-lock.yaml
	assert_success
}

@test "non-frozen pnpm re-resolve writes compatible patch identities" {
	cat >package.json <<'EOF'
{
  "name": "patch-reresolve-test",
  "private": true,
  "dependencies": {
    "is-odd": "3.0.1",
    "is-positive": "3.1.0"
  }
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - .
patchedDependencies:
  is-odd@3.0.1: patches/is-odd@3.0.1.patch
EOF
	mkdir patches
	cat >patches/is-odd@3.0.1.patch <<'EOF'
diff --git a/index.js b/index.js
index 79d1f22a8e7a27efb8841bb83cb682ea1ff3a59c..1e33b4cf949b73bde8861ad65de71b4e46360259 100644
--- a/index.js
+++ b/index.js
@@ -24,1 +24,2 @@ module.exports = function isOdd(value) {
 };
+module.exports.patched = 'v1';
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

patchedDependencies:
  is-odd@3.0.1: cc42c0f31ffe1fcce0f04529bb17d094cb5934577d038da76449d7b92364195c

importers:

  .:
    dependencies:
      is-odd:
        specifier: 3.0.1
        version: 3.0.1(patch_hash=cc42c0f31ffe1fcce0f04529bb17d094cb5934577d038da76449d7b92364195c)

packages:

  is-number@6.0.0:
    resolution: {integrity: sha512-Wu1VHeILBK8KAWJUAiSZQX94GmOE45Rg6/538fKwiloUu21KncEkYGPqob2oSZ5mUT73vLGrHQjKw3KMPwfDzg==}

  is-odd@3.0.1:
    resolution: {integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==}

snapshots:

  is-number@6.0.0: {}

  is-odd@3.0.1(patch_hash=cc42c0f31ffe1fcce0f04529bb17d094cb5934577d038da76449d7b92364195c):
    dependencies:
      is-number: 6.0.0
EOF

	run aube install --no-frozen-lockfile --ignore-scripts
	assert_success
	run node -e 'const odd = require("is-odd"); require("is-positive"); if (odd.patched !== "v1") process.exit(1)'
	assert_success

	patch_hash="$(awk '$1 == "is-odd@3.0.1:" && NF == 2 { print $2; exit }' pnpm-lock.yaml)"
	assert_equal "${#patch_hash}" 64
	run grep -Fq "version: 3.0.1(patch_hash=$patch_hash)" pnpm-lock.yaml
	assert_success
	run grep -Fq "is-odd@3.0.1(patch_hash=$patch_hash):" pnpm-lock.yaml
	assert_success
}
