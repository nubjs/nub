#!/usr/bin/env bats

# `aube peers check` reads the lockfile and reports unmet/missing peer
# dependencies. The fixture install at the top exercises the happy path
# (every declared peer satisfied); the no-lockfile case checks the
# friendly error.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube peers --help lists the check subcommand" {
	run aube peers --help
	assert_success
	assert_output --partial "check"
}

@test "aube peers check errors with a hint when no lockfile is present" {
	echo '{"name":"x","version":"1.0.0"}' >package.json
	run aube peers check
	assert_failure
	assert_output --partial "no lockfile found"
	assert_output --partial "aube install"
}

@test "aube peers check reports satisfied after installing a package with peer deps" {
	# use-sync-external-store declares peerDep react ^16.8 || ^17 || ^18,
	# and auto-install-peers will pull in a satisfying react. After install
	# every declared peer should be satisfied.
	cat >package.json <<'JSON'
{
  "name": "peers-check-test",
  "version": "1.0.0",
  "dependencies": {
    "use-sync-external-store": "1.2.0"
  }
}
JSON
	run aube install
	assert_success

	run aube peers check
	assert_success
	assert_output --partial "All peer dependencies are satisfied."
}

@test "aube peers check --json emits a JSON array" {
	echo '{"name":"x","version":"1.0.0"}' >package.json
	# Empty lockfile (just the version header) — no packages, so JSON
	# is `[]` and exit is 0.
	cat >aube-lock.yaml <<'YAML'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:
  .: {}
YAML
	run aube peers check --json
	assert_success
	assert_output "[]"
}

@test "dedupe-peers=true emits version-only suffixes in aube-lock.yaml" {
	# With dedupePeers on, peer suffixes in the lockfile switch from
	# `(react@18.x.y)` to `(18.x.y)`. We check for the exact version
	# the fixture pins so the assertion isn't tied to whatever latest
	# satisfies.
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
dedupePeers=true
EOF
	cat >package.json <<'JSON'
{
  "name": "dedupe-peers-test",
  "version": "1.0.0",
  "dependencies": {
    "react": "17.0.2",
    "use-sync-external-store": "1.2.0"
  }
}
JSON
	run aube install
	assert_success

	# Lockfile should contain a version-only peer suffix on
	# use-sync-external-store. The canonical default ships the name
	# form `(react@17.0.2)`; under dedupePeers=true it drops to
	# `(17.0.2)`.
	run grep -E "use-sync-external-store@1\.2\.0\(17\.0\.2\)" aube-lock.yaml
	assert_success

	run grep -E "use-sync-external-store@1\.2\.0\(react@17\.0\.2\)" aube-lock.yaml
	assert_failure
}

@test "dedupe-peer-dependents=false preserves peer-suffixed variants" {
	# The default dedupe collapses peer-equivalent subtree variants
	# down to one canonical entry. With it off, the full contextualized
	# graph is preserved. We check that the basic peer wiring still
	# produces a useable install and the lockfile's peer suffixes
	# still use the full `(name@version)` form (dedupePeers stays
	# off — this test only toggles dedupePeerDependents).
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
dedupePeerDependents=false
EOF
	cat >package.json <<'JSON'
{
  "name": "dedupe-peer-dependents-off",
  "version": "1.0.0",
  "dependencies": {
    "react": "17.0.2",
    "use-sync-external-store": "1.2.0"
  }
}
JSON
	run aube install
	assert_success
	assert_link_exists node_modules/react
	assert_link_exists node_modules/use-sync-external-store

	# Name-form peer suffix remains because dedupePeers is still the
	# default (false). Only dedupe_peer_dependents was toggled.
	run grep -E "use-sync-external-store@1\.2\.0\(react@17\.0\.2\)" aube-lock.yaml
	assert_success
}

@test "resolve-peers-from-workspace-root=false does not fall back to root deps" {
	# With the flag on (default), an unresolved peer can be picked up
	# from the root importer's dep. With it off, the fallback tier is
	# skipped — in this fixture the peer still resolves through the
	# existing graph-wide scan, so the install succeeds regardless.
	# What we're verifying is that turning the setting off doesn't
	# break the normal happy path.
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
resolvePeersFromWorkspaceRoot=false
EOF
	cat >package.json <<'JSON'
{
  "name": "resolve-peers-from-root-off",
  "version": "1.0.0",
  "dependencies": {
    "react": "17.0.2",
    "use-sync-external-store": "1.2.0"
  }
}
JSON
	run aube install
	assert_success
	assert_link_exists node_modules/react
	assert_link_exists node_modules/use-sync-external-store

	# Lockfile still contains react@17.0.2 and the peer wiring still
	# completes via the graph-wide scan tier.
	run grep -E "use-sync-external-store@1\.2\.0\(react@17\.0\.2\)" aube-lock.yaml
	assert_success
}

@test "flipping dedupe-peers reinstall produces updated lockfile" {
	# Install with the default (dedupePeers=false), flip the setting,
	# reinstall, and verify the lockfile now carries the version-only
	# suffix form. Guards against caching the previous suffix format
	# across `aube install` invocations.
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
EOF
	cat >package.json <<'JSON'
{
  "name": "dedupe-peers-flip",
  "version": "1.0.0",
  "dependencies": {
    "react": "17.0.2",
    "use-sync-external-store": "1.2.0"
  }
}
JSON
	run aube install
	assert_success
	# First install: the name-form suffix is written.
	run grep -E "use-sync-external-store@1\.2\.0\(react@17\.0\.2\)" aube-lock.yaml
	assert_success

	# Flip the setting, blow away the lockfile so the resolver
	# re-computes suffixes from scratch (an existing lockfile with the
	# canonical suffixes would otherwise reuse them verbatim).
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
dedupePeers=true
EOF
	rm -f aube-lock.yaml
	run aube install
	assert_success
	run grep -E "use-sync-external-store@1\.2\.0\(17\.0\.2\)" aube-lock.yaml
	assert_success
	run grep -E "use-sync-external-store@1\.2\.0\(react@17\.0\.2\)" aube-lock.yaml
	assert_failure
}
