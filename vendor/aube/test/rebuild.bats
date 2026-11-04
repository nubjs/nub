#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_write_pkg() {
	cat >package.json <<-EOF
		{
		  "name": "rebuild-fixture",
		  "version": "1.2.3",
		  "scripts": $1
		}
	EOF
}

@test "aube rebuild does not run the project's own lifecycle hooks" {
	# pnpm 12's `rebuild` builds dependencies only; the project's hooks
	# run on install. No auto-install on the stale tree either, which
	# would run them.
	_write_pkg '{"preinstall":"echo HOOK_RAN","install":"echo HOOK_RAN","postinstall":"echo HOOK_RAN","prepare":"echo HOOK_RAN"}'
	[ ! -d node_modules ]
	run aube rebuild
	assert_success
	refute_output --partial "HOOK_RAN"
	refute_output --partial "Auto-installing"
}

@test "aube rb is an alias for rebuild" {
	_write_pkg '{}'
	run aube rb
	assert_success
	refute_output --partial "Unknown"
}

@test "aube recursive rebuild does not run workspace packages' own hooks" {
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - packages/*
	EOF
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	mkdir -p packages/lib-a
	cat >packages/lib-a/package.json <<-'EOF'
		{
		  "name": "lib-a",
		  "version": "1.0.0",
		  "scripts": { "postinstall": "echo lib-a-rebuilt" }
		}
	EOF

	run aube recursive rebuild
	assert_success
	refute_output --partial "lib-a-rebuilt"
}

@test "aube rebuild re-runs allowlisted dependency lifecycle scripts" {
	cat >package.json <<'JSON'
{
  "name": "rebuild-dep-builds-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "echo root-pre > rebuild-order.log",
    "install": "test -f aube-builds-marker.txt && echo root-install >> rebuild-order.log",
    "postinstall": "echo root-post >> rebuild-order.log",
    "prepare": "echo root-prepare >> rebuild-order.log"
  },
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt

	rm aube-builds-marker.txt
	rm rebuild-order.log
	run aube rebuild
	assert_success
	assert_file_exists aube-builds-marker.txt
	run cat aube-builds-marker.txt
	assert_output "ran:aube-test-builds-marker@1.0.0"
	# The project's own hooks are install's, not rebuild's.
	assert_not_exists rebuild-order.log
}

@test "aube rebuild with readonly side-effects cache still re-runs dependency lifecycle scripts" {
	cat >package.json <<'JSON'
{
  "name": "rebuild-readonly-cache-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt

	cat >.npmrc <<'EOF'
sideEffectsCacheReadonly=true
EOF
	rm aube-builds-marker.txt
	run aube rebuild
	assert_success
	assert_file_exists aube-builds-marker.txt
	run cat aube-builds-marker.txt
	assert_output "ran:aube-test-builds-marker@1.0.0"
}

@test "aube rebuild <pkg> runs only the named dep's scripts" {
	cat >package.json <<'JSON'
{
  "name": "rebuild-selective-test",
  "version": "1.0.0",
  "scripts": {
    "preinstall": "echo root-pre >> rebuild-order.log",
    "install": "echo root-install >> rebuild-order.log",
    "postinstall": "echo root-post >> rebuild-order.log",
    "prepare": "echo root-prepare >> rebuild-order.log"
  },
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0",
    "aube-test-builds-marker-2": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true,
      "aube-test-builds-marker-2": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
	assert_file_exists aube-builds-marker-2.txt

	rm aube-builds-marker.txt aube-builds-marker-2.txt rebuild-order.log
	run aube rebuild aube-test-builds-marker
	assert_success
	# Only the named dep's marker is recreated.
	assert_file_exists aube-builds-marker.txt
	assert_not_exists aube-builds-marker-2.txt
	assert_not_exists rebuild-order.log
}

@test "aube rebuild <pkg-1> <pkg-2> runs both named deps and skips others" {
	cat >package.json <<'JSON'
{
  "name": "rebuild-multiple-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0",
    "aube-test-builds-marker-2": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true,
      "aube-test-builds-marker-2": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_file_exists aube-builds-marker.txt
	assert_file_exists aube-builds-marker-2.txt

	rm aube-builds-marker.txt aube-builds-marker-2.txt
	run aube rebuild aube-test-builds-marker aube-test-builds-marker-2
	assert_success
	assert_file_exists aube-builds-marker.txt
	assert_file_exists aube-builds-marker-2.txt
}

@test "aube rebuild <pkg> bypasses the build policy for the named dep" {
	# No `allowBuilds` entry, so the policy would skip the dep on a
	# default `aube rebuild` (no-args). Naming the dep is the explicit
	# opt-in.
	cat >package.json <<'JSON'
{
  "name": "rebuild-bypass-policy-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	# Pre-approve once for install so the dep is on disk; the rebuild
	# itself then runs without any allowlist on the manifest.
	run aube install --dangerously-allow-all-builds
	assert_success

	rm aube-builds-marker.txt
	run aube rebuild aube-test-builds-marker
	assert_success
	assert_file_exists aube-builds-marker.txt
}

@test "aube rebuild <unknown-pkg> errors with an unmatched-name message" {
	# Need a lockfile present so the unmatched-name check fires;
	# install pulls in a single dep we don't reference by name.
	cat >package.json <<'JSON'
{
  "name": "rebuild-unmatched-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success

	run aube rebuild not-a-real-dep
	assert_failure
	assert_output --partial "no installed dependency matches"
	assert_output --partial "not-a-real-dep"
}

@test "aube rebuild <pkg> errors when no lockfile is present" {
	# Selective rebuild without a lockfile would otherwise skip the
	# unmatched-name check and the root hooks both, exiting Ok silently.
	# Guard fires before the would-be no-op.
	cat >package.json <<'JSON'
{
  "name": "rebuild-no-lockfile-test",
  "version": "1.0.0"
}
JSON
	run aube rebuild some-package
	assert_failure
	assert_output --partial "no lockfile found"
	# Miette word-wraps the diagnostic at column boundaries that depend
	# on the temp dir path length, so split substrings that always live
	# on the same line as the surrounding text.
	assert_output --partial "before targeting"
}

@test "aube rebuild re-runs allowlisted dependency lifecycle scripts in hoisted mode" {
	cat >package.json <<'JSON'
{
  "name": "rebuild-hoisted-dep-builds-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-builds-marker": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
nodeLinker: hoisted
YAML
	run aube install
	assert_success
	assert_file_exists node_modules/aube-test-builds-marker/package.json
	assert_not_exists node_modules/.aube
	assert_file_exists aube-builds-marker.txt

	rm aube-builds-marker.txt
	run aube rebuild
	assert_success
	assert_file_exists aube-builds-marker.txt
	run cat aube-builds-marker.txt
	assert_output "ran:aube-test-builds-marker@1.0.0"
}

# Ported from pnpm/test/install/lifecycleScripts.ts:282
# ('selective rebuild preserves ignoredBuilds for packages not being rebuilt').
# Aube equivalent: pnpm's `node_modules/.modules.yaml.ignoredBuilds` is
# `.aube-state.unreviewed_builds`. Direct file inspection is brittle (the
# state file is BLAKE3-hashed shape) so assert via the warm-path repeat-
# install warning, which re-emits exactly when the unreviewed set is
# non-empty (see lifecycle_scripts.bats:942 for the canonical pattern).
@test "aube rebuild <pkg> preserves unreviewed_builds for packages not rebuilt" {
	cat >package.json <<'JSON'
{
  "name": "rebuild-preserve-unreviewed-test",
  "version": "1.0.0",
  "dependencies": {
    "@pnpm.e2e/pre-and-postinstall-scripts-example": "1.0.0",
    "@pnpm.e2e/install-script-example": "1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "@pnpm.e2e/pre-and-postinstall-scripts-example": true
    }
  }
}
JSON
	run aube install
	assert_success
	assert_output --partial "ignored build scripts"
	assert_output --partial "@pnpm.e2e/install-script-example"

	# Selectively rebuild only the approved dep.
	run aube rebuild @pnpm.e2e/pre-and-postinstall-scripts-example
	assert_success

	# Repeat install must still fire the unreviewed-builds warning for
	# the un-approved sibling — selective rebuild must not silently
	# clear the unreviewed set.
	run aube install
	assert_success
	assert_output --partial "ignored build scripts"
	assert_output --partial "@pnpm.e2e/install-script-example"
}
