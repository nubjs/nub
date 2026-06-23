#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Helper: set up a project with is-odd locked to an old version
_setup_outdated_project() {
	cat >package.json <<'EOF'
{
  "name": "test-update",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": ">=0.1.0"
  }
}
EOF

	cat >aube-lock.yaml <<'EOF'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .:
    dependencies:
      is-odd:
        specifier: '>=0.1.0'
        version: 0.1.2

packages:
  is-number@3.0.0:
    resolution: {integrity: sha512-4cboCqIpliH+mAvFNegjZQ4kgKc3ZUhQVr3HvWbSh5q3WH2v82ct+T2Y1hdU5Gdtorx/cLifQjqCbL7bpznLTg==}
  is-odd@0.1.2:
    resolution: {integrity: sha512-Ri7C2K7o5IrUU9UEI8losXJCCD/UtsaIrkR5sxIcFg4xQ9cRJXlWA5DQvTE0yDc0krvSNLsRGXN11UPS6KyfBw==}
  kind-of@3.2.2:
    resolution: {integrity: sha512-NOW9QQXMoZGg/oqnVNoNTTIFEIid1627WCffUBJEdMxYApq7mNE7CpzucIPc+ZQg25Phej7IJSmX3hO+oblOtQ==}

snapshots:
  is-number@3.0.0:
    dependencies:
      kind-of: 3.2.2
  is-odd@0.1.2:
    dependencies:
      is-number: 3.0.0
  kind-of@3.2.2: {}
EOF
}

@test "aube update: updates a named package to latest matching version" {
	_setup_outdated_project

	run aube update is-odd
	assert_success

	# Lockfile should now have a newer is-odd (3.x)
	run grep 'is-odd@3' aube-lock.yaml
	assert_success

	# node_modules should be populated
	assert_file_exists node_modules/is-odd/index.js
}

@test "aube update: reports version change in output" {
	_setup_outdated_project

	run aube update is-odd
	assert_success
	# Should report the version bump
	assert_output --partial '0.1.2 ->'
}

@test "aube update: all deps updates everything" {
	_setup_outdated_project

	run aube update
	assert_success

	# Should update is-odd
	run grep 'is-odd@3' aube-lock.yaml
	assert_success
}

@test "aube update --interactive: requires a TTY instead of updating everything" {
	_setup_outdated_project

	run aube update --interactive --latest
	assert_failure
	assert_output --partial "requires stdin and stderr to be TTYs"

	run grep 'is-odd@0.1.2' aube-lock.yaml
	assert_success
	run grep '>=0.1.0' package.json
	assert_success
}

@test "aube update: skips registry for package.json workspace deps" {
	cat >package.json <<'EOF'
{"workspaces":["sub"],"dependencies":{"happy-sunny-hippo":"workspace:"}}
EOF
	mkdir sub
	cat >sub/package.json <<'EOF'
{"name":"happy-sunny-hippo"}
EOF

	run aube update
	assert_success
	refute_output --partial "package not found"
	assert_file_exists node_modules/happy-sunny-hippo/package.json
}

@test "aube update --latest: preserves catalog manifest specifiers" {
	cat >package.json <<'EOF'
{
  "name": "test-update-catalog",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "catalog:"
  }
}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - "."
catalog:
  is-odd: ^3.0.1
EOF

	run aube update --latest
	assert_success

	run grep '"is-odd": "catalog:"' package.json
	assert_success
	run grep "specifier: 'catalog:'" aube-lock.yaml
	assert_success
}

@test "aube update: updateConfig.ignoreDependencies skips all-deps updates" {
	_setup_outdated_project
	cat >package.json <<'EOF'
{
  "name": "test-update",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": ">=0.1.0"
  },
  "updateConfig": {
    "ignoreDependencies": ["is-odd"]
  }
}
EOF

	run aube update
	assert_success
	run grep 'is-odd@0.1.2' aube-lock.yaml
	assert_success
}

@test "aube update: workspace updateConfig.ignoreDependencies skips all-deps updates" {
	_setup_outdated_project
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - "."
updateConfig:
  ignoreDependencies:
    - is-odd
EOF

	run aube update
	assert_success
	run grep 'is-odd@0.1.2' aube-lock.yaml
	assert_success
}

@test "aube update: explicit ignored dependency errors" {
	_setup_outdated_project
	cat >>.npmrc <<'EOF'
updateConfig.ignoreDependencies=["is-odd"]
EOF

	run aube update is-odd
	assert_failure
	assert_output --partial "ignored by updateConfig.ignoreDependencies"
}

@test "aube update: reports already latest when nothing to update" {
	cat >package.json <<'EOF'
{
  "name": "test-update-noop",
  "version": "0.0.0",
  "dependencies": {}
}
EOF

	# Install first to get a lockfile
	run aube add is-odd
	assert_success

	# Update should report "already latest"
	run aube update is-odd
	assert_success
	assert_output --partial 'already latest'
}

@test "aube update: errors on unknown package" {
	cat >package.json <<'EOF'
{
  "name": "test-update-unknown",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.0"
  }
}
EOF

	run aube update nonexistent-pkg
	assert_failure
	assert_output --partial "not a dependency"
}

@test "aube update: preserves package.json specifiers" {
	_setup_outdated_project

	run aube update is-odd
	assert_success

	# package.json should still have the original specifier
	run grep '>=0.1.0' package.json
	assert_success
}

@test "aube update --latest --no-save: bumps the lockfile but not package.json" {
	_setup_outdated_project

	run aube update --latest --no-save is-odd
	assert_success
	assert_output --partial 'Skipping package.json update (--no-save)'

	# package.json range stays exactly as the user wrote it.
	run grep '>=0.1.0' package.json
	assert_success

	# The lockfile picked up a newer version than 0.1.2 (the seed pin).
	run grep -c 'is-odd@3' aube-lock.yaml
	assert_success
}

@test "aube update --lockfile-only: refreshes lockfile without populating node_modules" {
	_setup_outdated_project

	run aube update --lockfile-only is-odd
	assert_success

	# Lockfile picks up a newer is-odd than the seeded 0.1.2 pin.
	run grep 'is-odd@3' aube-lock.yaml
	assert_success

	# node_modules is not materialized.
	assert [ ! -e node_modules ]
}

@test "aube update --lockfile-only --latest: bumps direct deps without linking" {
	_setup_outdated_project

	run aube update --lockfile-only --latest is-odd
	assert_success

	# package.json gets the manifest rewrite (--latest flag).
	run grep '"is-odd"' package.json
	assert_success
	refute_output --partial '>=0.1.0'

	# Lockfile is fresh, but no node_modules.
	run grep 'is-odd@3' aube-lock.yaml
	assert_success
	assert [ ! -e node_modules ]
}

@test "aube update --lockfile-only conflicts with --frozen-lockfile" {
	_setup_outdated_project
	run aube update --lockfile-only --frozen-lockfile
	assert_failure
}

# Regression for discussion #345 (mrazauskas): `aube update` was
# stripping non-host platform-locked optional deps from the lockfile
# because `super::build_resolver` constructed a stripped-down resolver
# that never received `supportedArchitectures` from install's settings
# pipeline. The bug collapsed packages like `@biomejs/biome` /
# `rollup` to one platform binary each, breaking cross-platform CI.
@test "aube update preserves cross-platform optional deps in lockfile" {
	cat >package.json <<-'JSON'
		{
		  "name": "update-cross-platform",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# Sanity: install widens supportedArchitectures and writes the
	# win32-only optional into the committed lockfile even on
	# Linux/macOS hosts. Without this baseline the regression test
	# below has nothing to preserve.
	run grep -F 'aube-test-optional-win32@1.0.0' aube-lock.yaml
	assert_success

	# The actual regression: `aube update` should re-resolve under
	# the same widened platform filter install used, so the optional
	# entry survives the rewrite.
	run aube update
	assert_success
	run grep -F 'aube-test-optional-win32@1.0.0' aube-lock.yaml
	assert_success
}

# Companion regression for discussion #345: under `resolution-mode=time-based`
# `aube update` was dropping `time:` entries for direct deps from the
# rewritten lockfile because (a) the stripped-down `build_resolver`
# skipped install-time settings and (b) `aube update`'s `filtered_existing`
# strips direct deps from `existing.packages` to force a fresh re-resolve,
# so the lockfile-reuse path's time-carry-forward never fired for them.
# Transitive deps stayed in `existing.packages`; the writer prunes their
# publish times for pnpm v11 parity. pnpm only persists the `time:` block
# in time-based mode (resolveDependencies.ts populates `time` solely in the
# `time-based` branch), so this preservation behavior is asserted there.
@test "aube update preserves time: entries for direct deps (time-based mode)" {
	# Use is-odd@^3.0.1 (-> is-number@6.0.0): in time-based mode the
	# cutoff is derived from the direct dep's publish time, and that
	# direct must be newer than its transitives or they get filtered
	# out. is-odd@3.0.1 is newer than is-number@6.0.0 (the inverse
	# is-odd@0.1.2 -> is-number@3.0.0 tree fails to resolve under the
	# cutoff). `^3.0.1` pins to the only 3.x in the fixture, so the
	# re-resolve lands back on 3.0.1 and the `time:` entry round-trips.
	cat >package.json <<-'JSON'
		{
		  "name": "update-time-preserve",
		  "version": "0.0.0",
		  "dependencies": {
		    "is-odd": "^3.0.1"
		  }
		}
	JSON
	# Append so the registry= line _common_setup wrote stays in place.
	cat >>.npmrc <<-'EOF'
		resolution-mode=time-based
	EOF
	cat >aube-lock.yaml <<-'EOF'
		lockfileVersion: '9.0'

		settings:
		  autoInstallPeers: true
		  excludeLinksFromLockfile: false

		time:
		  is-odd@3.0.1: '2099-01-02T00:00:00.000Z'
		  is-number@6.0.0: '2099-01-03T00:00:00.000Z'

		importers:
		  .:
		    dependencies:
		      is-odd:
		        specifier: ^3.0.1
		        version: 3.0.1

		packages:
		  is-number@6.0.0:
		    resolution: {integrity: sha512-Wu1VHeILBK8KAWJUAiSZQX94GmOE45Rg6/538fKwiloUu21KncEkYGPqob2oSZ5mUT73vLGrHQjKw3KMPwfDzg==}
		  is-odd@3.0.1:
		    resolution: {integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==}

		snapshots:
		  is-number@6.0.0: {}
		  is-odd@3.0.1:
		    dependencies:
		      is-number: 6.0.0
	EOF

	run aube update
	assert_success

	# The direct dep must keep a `time:` entry after the rewrite. We
	# can't pin the timestamp value because the resolver may have
	# refreshed it from the packument during re-resolve, but the entry
	# MUST be present. pnpm v11 prunes transitive publish times from the
	# written lockfile, so the transitive is-number@6.0.0 must be gone.
	#
	time_block="$(awk '/^time:/{f=1;next} /^[^[:space:]].*:/{f=0} f' aube-lock.yaml)"
	run bash -c "grep -E '^  is-odd@3\\.0\\.1: [^[:space:]]' <<<\"\$1\"" _ "$time_block"
	assert_success
	run bash -c "grep -E '^  is-number@6\\.0\\.0: [^[:space:]]' <<<\"\$1\"" _ "$time_block"
	assert_failure
}

# Default (highest) resolution must drop a stray `time:` block on rewrite,
# matching pnpm: outside time-based mode pnpm never assigns `newLockfile.time`,
# so a lockfile that picked up a `time:` block elsewhere (an older aube, a
# time-based install, a hand edit) is cleaned up on the next update. Guards the
# fix for the bug where `minimumReleaseAge`/`trustPolicy` leaked a `time:` block
# into highest-mode lockfiles.
@test "aube update drops a stray time: block under default resolution (pnpm parity)" {
	cat >package.json <<-'JSON'
		{
		  "name": "update-time-drop",
		  "version": "0.0.0",
		  "dependencies": {
		    "is-odd": "^0.1.0"
		  }
		}
	JSON
	cat >aube-lock.yaml <<-'EOF'
		lockfileVersion: '9.0'

		settings:
		  autoInstallPeers: true
		  excludeLinksFromLockfile: false

		time:
		  is-odd@0.1.2: '2099-01-02T00:00:00.000Z'
		  is-number@3.0.0: '2099-01-03T00:00:00.000Z'
		  kind-of@3.2.2: '2099-01-04T00:00:00.000Z'

		importers:
		  .:
		    dependencies:
		      is-odd:
		        specifier: ^0.1.0
		        version: 0.1.2

		packages:
		  is-number@3.0.0:
		    resolution: {integrity: sha512-4cboCqIpliH+mAvFNegjZQ4kgKc3ZUhQVr3HvWbSh5q3WH2v82ct+T2Y1hdU5Gdtorx/cLifQjqCbL7bpznLTg==}
		  is-odd@0.1.2:
		    resolution: {integrity: sha512-Ri7C2K7o5IrUU9UEI8losXJCCD/UtsaIrkR5sxIcFg4xQ9cRJXlWA5DQvTE0yDc0krvSNLsRGXN11UPS6KyfBw==}
		  kind-of@3.2.2:
		    resolution: {integrity: sha512-NOW9QQXMoZGg/oqnVNoNTTIFEIid1627WCffUBJEdMxYApq7mNE7CpzucIPc+ZQg25Phej7IJSmX3hO+oblOtQ==}

		snapshots:
		  is-number@3.0.0:
		    dependencies:
		      kind-of: 3.2.2
		  is-odd@0.1.2:
		    dependencies:
		      is-number: 3.0.0
		  kind-of@3.2.2: {}
	EOF

	run aube update
	assert_success
	run grep -E '^time:' aube-lock.yaml
	assert_failure
}
