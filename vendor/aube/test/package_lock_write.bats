#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube install preserves package-lock.json format on re-resolve (does not create aube-lock.yaml)" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	# Force a fresh resolve so the writer runs. --no-frozen-lockfile
	# re-resolves unconditionally and writes back whichever format the
	# project currently uses.
	run aube install --no-frozen-lockfile
	assert_success

	assert_file_exists package-lock.json
	# pnpm-lock.yaml must NOT appear — preserving the project's existing
	# lockfile format is the whole point of this feature.
	assert [ ! -f pnpm-lock.yaml ]
	assert [ ! -f aube-lock.yaml ]

	# The rewritten file must still be readable as a v3 lockfile and
	# install must succeed from it.
	run grep -c '"lockfileVersion": 3' package-lock.json
	assert_success
	run grep -c '"node_modules/is-odd"' package-lock.json
	assert_success

	# Second install from the regenerated lockfile must succeed and
	# produce node_modules — confirms the writer emits a lockfile our
	# own parser can consume round-trip.
	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install preserves yarn.lock format on re-resolve (does not create aube-lock.yaml)" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube install --no-frozen-lockfile
	assert_success

	assert_file_exists yarn.lock
	assert [ ! -f pnpm-lock.yaml ]
	assert [ ! -f aube-lock.yaml ]

	# The rewritten file is still yarn v1: check the signature line
	# exists and that known packages appear as block headers.
	run grep -c '# yarn lockfile v1' yarn.lock
	assert_success
	run grep -c '"is-odd@3.0.1"' yarn.lock
	assert_success

	# Second install from the regenerated yarn.lock must succeed.
	# The writer emits multi-spec block headers (exact `name@version`
	# *plus* the manifest range), so reparse finds direct deps via
	# the manifest lookup even when the fixture uses range specs.
	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install preserves bun.lock format on re-resolve (does not create aube-lock.yaml)" {
	cp "$PROJECT_ROOT/fixtures/import-bun/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun/bun.lock" .

	run aube install --no-frozen-lockfile
	assert_success

	assert_file_exists bun.lock
	assert [ ! -f pnpm-lock.yaml ]
	assert [ ! -f aube-lock.yaml ]

	run grep -c '"lockfileVersion": 1' bun.lock
	assert_success
	# Second install from the regenerated bun.lock should succeed.
	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install preserves pnpm-lock.yaml format on re-resolve (does not create aube-lock.yaml)" {
	cp "$PROJECT_ROOT/fixtures/basic/package.json" .
	cp "$PROJECT_ROOT/fixtures/basic/pnpm-lock.yaml" .

	run aube install --no-frozen-lockfile
	assert_success

	assert_file_exists pnpm-lock.yaml
	assert [ ! -f aube-lock.yaml ]

	run grep -c "lockfileVersion: '9.0'" pnpm-lock.yaml
	assert_success

	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "aube install --lockfile-only preserves package-lock.json format" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	# The --lockfile-only short-circuit writes the lockfile and exits
	# before the main install pipeline runs. It must use the same
	# format-preserving write path as the re-resolve branch.
	run aube install --lockfile-only --no-frozen-lockfile
	assert_success

	assert_file_exists package-lock.json
	assert [ ! -f aube-lock.yaml ]
	assert [ ! -f pnpm-lock.yaml ]
	assert [ ! -d node_modules ]

	run grep -c '"lockfileVersion": 3' package-lock.json
	assert_success
}

@test "aube install rewrites package-lock.json after manifest drift" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube install
	assert_success

	# Drift: tweak a direct-dep specifier. The next install re-resolves
	# and must write back to package-lock.json, not pnpm-lock.yaml.
	# (Using the same resolved version so the registry fixture can serve it.)
	node -e "const fs=require('fs');const p=JSON.parse(fs.readFileSync('package.json'));p.dependencies['is-odd']='3.0.1';fs.writeFileSync('package.json',JSON.stringify(p,null,2));"

	run aube install --no-frozen-lockfile
	assert_success

	assert_file_exists package-lock.json
	assert [ ! -f pnpm-lock.yaml ]
	assert [ ! -f aube-lock.yaml ]
	run grep -c '"lockfileVersion": 3' package-lock.json
	assert_success
}

@test "aube add preserves existing pnpm-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/basic/package.json" .
	cp "$PROJECT_ROOT/fixtures/basic/pnpm-lock.yaml" .

	run aube add is-number
	assert_success

	assert_file_exists pnpm-lock.yaml
	assert [ ! -f aube-lock.yaml ]
	run grep -F "is-number:" pnpm-lock.yaml
	assert_success
}

@test "aube remove preserves existing pnpm-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/basic/package.json" .
	cp "$PROJECT_ROOT/fixtures/basic/pnpm-lock.yaml" .

	run aube remove is-even
	assert_success

	assert_file_exists pnpm-lock.yaml
	assert [ ! -f aube-lock.yaml ]
	run grep -F "is-even:" pnpm-lock.yaml
	assert_failure
}

@test "aube update preserves existing pnpm-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/basic/package.json" .
	cp "$PROJECT_ROOT/fixtures/basic/pnpm-lock.yaml" .

	run aube update is-odd
	assert_success

	assert_file_exists pnpm-lock.yaml
	assert [ ! -f aube-lock.yaml ]
	run grep -F "is-odd:" pnpm-lock.yaml
	assert_success
}

@test "aube run does not re-install after package-lock.json write" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube install --no-frozen-lockfile
	assert_success

	# A subsequent run must NOT print "Auto-installing" (no re-install loop)
	run aube run --if-present some-nonexistent-script
	assert_success
	refute_output --partial "Auto-installing"
}

@test "aube run does not re-install after yarn.lock write" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube install --no-frozen-lockfile
	assert_success

	run aube run --if-present some-nonexistent-script
	assert_success
	refute_output --partial "Auto-installing"
}

@test "aube run does not re-install after bun.lock write" {
	cp "$PROJECT_ROOT/fixtures/import-bun/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun/bun.lock" .

	run aube install --no-frozen-lockfile
	assert_success

	run aube run --if-present some-nonexistent-script
	assert_success
	refute_output --partial "Auto-installing"
}

@test "aube dedupe preserves existing pnpm-lock.yaml" {
	cat >package.json <<'EOF'
{
  "name": "aube-test-dedupe-pnpm",
  "version": "1.0.0",
  "dependencies": { "is-odd": "^3.0.1" }
}
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'
settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false
importers:
  .:
    dependencies:
      is-odd:
        specifier: ^3.0.1
        version: 3.0.1
packages:
  is-odd@3.0.1:
    resolution:
      integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==
  is-number@6.0.0:
    resolution:
      integrity: sha512-Wu1VHeILBK8KAWJUAiSZQX94GmOE45Rg6/538fKwiloUu21KncEkYGPqob2oSZ5mUT73vLGrHQjKw3KMPwfDzg==
  orphan-pkg@1.0.0:
    resolution:
      integrity: sha512-0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000==
snapshots:
  is-odd@3.0.1:
    dependencies:
      is-number: 6.0.0
  is-number@6.0.0: {}
  orphan-pkg@1.0.0: {}
EOF

	run aube dedupe
	assert_success

	assert_file_exists pnpm-lock.yaml
	assert [ ! -f aube-lock.yaml ]
	run grep -F "orphan-pkg" pnpm-lock.yaml
	assert_failure
}

# Regression: `aube install` must handle npm-style aliases
# (`"<alias>": "npm:<real>@..."`) captured in package-lock.json. npm
# writes the entry at `node_modules/<alias>` with `name: "<real>"` and
# a `resolved:` URL pointing at the *real* package; aube used to treat
# the alias as the registry name and 404 on fetch. This verifies that
# install from the captured lockfile completes and that the alias
# lands as a distinct top-level folder, not as the real package name.
#
# Covers the lockfile-driven path (default / `--frozen-lockfile`).
# A separate fresh-resolve path through the resolver does not yet
# preserve the alias-as-folder-name — see the TODO in
# `aube-resolver::task.name` rewriting for `npm:` specifiers.
@test "aube install handles npm-alias in package-lock.json" {
	cat >package.json <<'JSON'
{
  "name": "alias-via-npm-lock",
  "version": "1.0.0",
  "dependencies": {
    "odd-alias": "npm:is-odd@3.0.1"
  }
}
JSON

	cat >package-lock.json <<'JSON'
{
  "name": "alias-via-npm-lock",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "alias-via-npm-lock",
      "version": "1.0.0",
      "dependencies": { "odd-alias": "npm:is-odd@3.0.1" }
    },
    "node_modules/is-number": {
      "version": "6.0.0",
      "resolved": "https://registry.npmjs.org/is-number/-/is-number-6.0.0.tgz",
      "integrity": "sha512-Wu1VHeILBK8KAWJUAiSZQX94GmOE45Rg6/538fKwiloUu21KncEkYGPqob2oSZ5mUT73vLGrHQjKw3KMPwfDzg=="
    },
    "node_modules/odd-alias": {
      "name": "is-odd",
      "version": "3.0.1",
      "resolved": "https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz",
      "integrity": "sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==",
      "dependencies": { "is-number": "^6.0.0" }
    }
  }
}
JSON

	run aube install
	assert_success

	# The alias is the folder name that node_modules walks see — not
	# the real package name. A top-level `is-odd` here would mean the
	# installer collapsed the alias back to the real name, breaking
	# any `require("odd-alias")` callers.
	assert_link_exists node_modules/odd-alias
	assert_not_exists node_modules/is-odd

	# Virtual store entry goes under the alias so transitive
	# lookups (`require("odd-alias")` from anywhere in the tree)
	# resolve via the same folder name declared in package.json.
	assert_dir_exists node_modules/.aube

	# The lockfile's `name:` + `resolved:` fields must survive the
	# pass-through. Without them a reparse has no way to recover the
	# real registry identity from the alias-qualified install path.
	run grep -F '"name": "is-odd"' package-lock.json
	assert_success
	run grep -F 'https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz' package-lock.json
	assert_success
	assert [ ! -f aube-lock.yaml ]
}

# npm lockfiles record peer deps on each package entry but emit a flat,
# pre-hoisted tree — Node's upward `node_modules/` walk finds peers at
# runtime. aube's isolated virtual store layout can't walk upward like
# that; it needs peer deps wired as explicit siblings in the peering
# package's `.aube/<dep_path>/node_modules/`. Without running the
# resolver's `apply_peer_contexts` pass over the parsed npm graph,
# every peer-dependent package (e.g. `@tanstack/devtools-vite` peering
# on `vite`) installs without its peer sibling and dies at runtime
# with `Cannot find package 'vite'`.
@test "aube install wires peer dep siblings from package-lock.json" {
	# Synthetic setup: declare a peer in the lockfile (using existing
	# offline fixtures is-odd + is-number — is-odd doesn't actually
	# peer on is-number, but the install path doesn't care; what we
	# care about is that the peer pass sees the declaration and
	# produces a contextualized sibling symlink).
	cat >package.json <<'JSON'
{
  "name": "peer-sibling-test",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1",
    "is-number": "6.0.0"
  }
}
JSON

	cat >package-lock.json <<'JSON'
{
  "name": "peer-sibling-test",
  "version": "1.0.0",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "name": "peer-sibling-test",
      "version": "1.0.0",
      "dependencies": { "is-odd": "3.0.1", "is-number": "6.0.0" }
    },
    "node_modules/is-odd": {
      "version": "3.0.1",
      "resolved": "https://registry.npmjs.org/is-odd/-/is-odd-3.0.1.tgz",
      "integrity": "sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==",
      "peerDependencies": { "is-number": "^6.0.0" }
    },
    "node_modules/is-number": {
      "version": "6.0.0",
      "resolved": "https://registry.npmjs.org/is-number/-/is-number-6.0.0.tgz",
      "integrity": "sha512-Wu1VHeILBK8KAWJUAiSZQX94GmOE45Rg6/538fKwiloUu21KncEkYGPqob2oSZ5mUT73vLGrHQjKw3KMPwfDzg=="
    }
  }
}
JSON

	run aube install
	assert_success

	# is-odd should now live at a peer-contextualized `.aube/` path.
	# Use a glob because the context suffix gets hashed into the
	# directory name and we don't want to hardcode the exact hash.
	# `.aube/<entry>` is a symlink into the global virtual store, so
	# `find` needs `-L` to follow it (`-type d` alone wouldn't match).
	is_odd_dir="$(find -L node_modules/.aube -maxdepth 1 -type d -name 'is-odd@3.0.1*' 2>/dev/null | head -1)"
	assert [ -n "$is_odd_dir" ]

	# The peer must be wired as a sibling inside is-odd's
	# `node_modules/`. Without the peer pass the directory only
	# contains `is-odd/` and Node's require("is-number") from is-odd
	# fails under the isolated layout.
	assert [ -e "$is_odd_dir/node_modules/is-number" ]
	assert [ -e "$is_odd_dir/node_modules/is-odd" ]
}
