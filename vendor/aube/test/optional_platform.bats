#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# The fixture `aube-test-optional-win32` declares `os: ["win32"]` so on
# Linux and macOS CI it must be skipped silently rather than failing
# the install. This mirrors pnpm's "graceful failure" for optional deps
# with unsatisfiable platform constraints.

@test "optional dep with win32-only os is skipped on non-win32 host" {
	cat >package.json <<-'JSON'
		{
		  "name": "optional-platform-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# Platform-mismatched optional should not land in node_modules.
	assert_not_exists node_modules/aube-test-optional-win32
}

@test "aube-lock.yaml captures cross-platform optional natives by default" {
	# When aube writes its native lockfile format it widens the resolver's
	# platform filter so Linux / Windows / arm64 optional natives are
	# recorded alongside the host's — a lockfile resolved on one platform
	# must install cleanly on every other platform without the user
	# hand-rolling `pnpm.supportedArchitectures`.
	cat >package.json <<-'JSON'
		{
		  "name": "cross-platform-lockfile-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_exists aube-lock.yaml
	# win32-only optional must still not be linked on non-win32 hosts…
	assert_not_exists node_modules/aube-test-optional-win32
	# …but it MUST land in the committed lockfile so a Windows CI run
	# resolving from this file picks it up.
	run grep -F 'aube-test-optional-win32@1.0.0' aube-lock.yaml
	assert_success
	# A package reachable only through an optional edge must carry
	# `optional: true` in the snapshots block, exactly as pnpm marks it.
	run grep -A1 -F 'aube-test-optional-win32@1.0.0:' aube-lock.yaml
	assert_output --partial 'optional: true'
}

@test "required platform-mismatched dep still gets fetched and linked" {
	# Regression guard for the catch-up fetch pass. Streaming fetch
	# defers platform-mismatched tarballs because filter_graph usually
	# drops them — but filter_graph only prunes *optional* edges. A
	# required dep that declares `os: ["win32"]` on a non-Windows host
	# survives the graph trim, so its tarball must be fetched after the
	# stream closes or the linker errors out with a missing store index.
	# This fixture is declared in `dependencies` (NOT optional) so we
	# exercise the exact code path cursor[bot] flagged. pnpm installs
	# the same package the same way (with a warning); aube matches.
	cat >package.json <<-'JSON'
		{
		  "name": "required-platform-mismatch-test",
		  "version": "0.0.0",
		  "dependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# The required-platform-mismatched dep must still land in
	# node_modules: aube honors pnpm's `packageIsInstallable` semantics
	# for required deps, and the catch-up fetch guarantees the store
	# index is present when the linker runs.
	assert_exists node_modules/aube-test-optional-win32/package.json
}

@test "pnpm-lock.yaml captures cross-platform optionals like pnpm does" {
	# pnpm itself records every optional-dep variant in pnpm-lock.yaml
	# regardless of the host running the resolve — that's how teammates
	# on other platforms install from a Linux-generated lockfile without
	# "Cannot find native binding" errors on macOS. aube matches.
	# Reported in discussion #174.
	: >pnpm-lock.yaml
	cat >package.json <<-'JSON'
		{
		  "name": "pnpm-lock-cross-platform",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_exists pnpm-lock.yaml
	assert_not_exists aube-lock.yaml
	# The win32-only optional must not land in node_modules on non-win32
	# hosts…
	assert_not_exists node_modules/aube-test-optional-win32
	# …but it MUST land in the lockfile's `packages:` block so a Windows
	# CI run resolving from this file picks it up.
	run grep -F 'aube-test-optional-win32@1.0.0:' pnpm-lock.yaml
	assert_success
	# pnpm flags optional-only packages with `optional: true` in the
	# snapshots block; aube matches so a parse-then-write round-trip and a
	# fresh resolve produce byte-identical snapshots.
	run grep -A1 -F 'aube-test-optional-win32@1.0.0:' pnpm-lock.yaml
	assert_output --partial 'optional: true'
}

@test "pnpm-lock.yaml records optional natives for exotic arches (full pnpm parity)" {
	# pnpm records EVERY optional-dep variant a package declares — even
	# rare triples like linux-ppc64. This fixture declares `cpu: ["ppc64"]`
	# and must survive into pnpm-lock.yaml under the full accept-all set —
	# the actual fix for the rollup optionalDependencies parity report.
	: >pnpm-lock.yaml
	cat >package.json <<-'JSON'
		{
		  "name": "pnpm-lock-exotic-arch",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-ppc64": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_exists pnpm-lock.yaml
	# Never installed on the x64/arm64 CI host…
	assert_not_exists node_modules/aube-test-optional-ppc64
	# …but recorded in the lockfile's `packages:` block, matching pnpm.
	run grep -F 'aube-test-optional-ppc64@1.0.0:' pnpm-lock.yaml
	assert_success
}

@test "aube-lock.yaml records optional natives for exotic arches (full pnpm parity)" {
	# aube-lock.yaml records EVERY optional-dep variant a package declares,
	# matching pnpm and bun — both write all natives (linux-ppc64, freebsd,
	# s390x, …) into their committed lockfiles. A teammate on a rare
	# platform installing from this file must find their native binding, so
	# nothing platform-specific is withheld. Install-time filtering still
	# keeps the host's node_modules trimmed: the dep is recorded, not linked.
	cat >package.json <<-'JSON'
		{
		  "name": "aube-lock-exotic-arch",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-ppc64": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_exists aube-lock.yaml
	assert_not_exists node_modules/aube-test-optional-ppc64
	run grep -F 'aube-test-optional-ppc64@1.0.0' aube-lock.yaml
	assert_success
}

@test "package-lock.json captures cross-platform optionals like npm does" {
	# npm records every optional-dep variant in package-lock.json with
	# its os/cpu fields, host notwithstanding — and `npm ci` *requires*
	# an entry for every root optionalDependency even when it doesn't
	# match the host (EUSAGE "Missing: fsevents@x.y.z from lock file"
	# otherwise), so a host-only package-lock written on Linux is
	# rejected by npm on Linux itself. aube matches npm's shape.
	cat >package-lock.json <<-'JSON'
		{
		  "name": "npm-lock-cross-platform",
		  "version": "0.0.0",
		  "lockfileVersion": 3,
		  "requires": true,
		  "packages": {
		    "": {
		      "name": "npm-lock-cross-platform",
		      "version": "0.0.0"
		    }
		  }
		}
	JSON
	cat >package.json <<-'JSON'
		{
		  "name": "npm-lock-cross-platform",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_not_exists aube-lock.yaml
	# The win32-only optional must not land in node_modules on non-win32
	# hosts…
	assert_not_exists node_modules/aube-test-optional-win32
	# …but it MUST get a packages entry so `npm ci` accepts the file.
	run grep -F '"node_modules/aube-test-optional-win32"' package-lock.json
	assert_success
}

@test "pnpm.supportedArchitectures widens the match set" {
	cat >package.json <<-'JSON'
		{
		  "name": "supported-arch-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "pnpm": {
		    "supportedArchitectures": {
		      "os": ["current", "win32"]
		    }
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# With win32 added to the supported set, the optional dep must be
	# installed even on Linux/macOS.
	assert_exists node_modules/aube-test-optional-win32
}

@test "pnpm-workspace.yaml supportedArchitectures widens the match set" {
	# pnpm v10 lets users declare `supportedArchitectures` in
	# pnpm-workspace.yaml so one shared config spans every importer.
	cat >package.json <<-'JSON'
		{
		  "name": "supported-arch-ws-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages: []
		supportedArchitectures:
		  os: ["current", "win32"]
	YAML
	run aube install --no-frozen-lockfile
	assert_success
	assert_exists node_modules/aube-test-optional-win32
}

@test "pnpm-workspace.yaml ignoredOptionalDependencies drops a named optional dep" {
	cat >package.json <<-'JSON'
		{
		  "name": "ignored-optional-ws-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  }
		}
	JSON
	cat >pnpm-workspace.yaml <<-'YAML'
		packages: []
		supportedArchitectures:
		  os: ["current", "win32"]
		ignoredOptionalDependencies:
		  - aube-test-optional-win32
	YAML
	run aube install --no-frozen-lockfile
	assert_success
	# ignoredOptionalDependencies from the workspace yaml wins even
	# though the platform filter would otherwise let the dep through.
	assert_not_exists node_modules/aube-test-optional-win32
}

@test "aube.supportedArchitectures widens the match set" {
	cat >package.json <<-'JSON'
		{
		  "name": "supported-arch-aube-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "aube": {
		    "supportedArchitectures": {
		      "os": ["current", "win32"]
		    }
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# `aube.*` is the native namespace — full parity with `pnpm.*`.
	assert_exists node_modules/aube-test-optional-win32
}

@test "aube.ignoredOptionalDependencies drops a named optional dep" {
	cat >package.json <<-'JSON'
		{
		  "name": "ignored-optional-aube-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "aube": {
		    "supportedArchitectures": { "os": ["current", "win32"] },
		    "ignoredOptionalDependencies": ["aube-test-optional-win32"]
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_not_exists node_modules/aube-test-optional-win32
}

@test "pnpm.ignoredOptionalDependencies drops a named optional dep" {
	cat >package.json <<-'JSON'
		{
		  "name": "ignored-optional-test",
		  "version": "0.0.0",
		  "optionalDependencies": {
		    "aube-test-optional-win32": "1.0.0"
		  },
		  "pnpm": {
		    "supportedArchitectures": { "os": ["current", "win32"] },
		    "ignoredOptionalDependencies": ["aube-test-optional-win32"]
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	# ignoredOptionalDependencies wins even when the platform filter
	# would otherwise allow it through.
	assert_not_exists node_modules/aube-test-optional-win32
}
