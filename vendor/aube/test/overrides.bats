#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Sanity baseline: with no override, is-odd@3.0.1's `is-number: ^6.0.0`
# range resolves to is-number@6.0.0 (the highest matching version in
# the fixture registry).
@test "without overrides, is-odd@3.0.1 resolves is-number to 6.0.0" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@6.0.0
}

@test "top-level overrides pin a transitive dep" {
	# is-odd@3.0.1 normally pulls is-number@^6.0.0. Pin it to 7.0.0.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
	# The original 6.0.0 should *not* be in the store layout for this project.
	run test -d node_modules/.aube/is-number@6.0.0
	assert_failure
}

@test "pnpm.overrides is honored" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "pnpm": { "overrides": { "is-number": "7.0.0" } }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "aube.overrides is honored" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "aube": { "overrides": { "is-number": "7.0.0" } }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "aube.overrides wins over pnpm.overrides on conflict" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "pnpm": { "overrides": { "is-number": "6.0.0" } },
		  "aube": { "overrides": { "is-number": "7.0.0" } }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
	run test -d node_modules/.aube/is-number@6.0.0
	assert_failure
}

@test "packageExtensions adds missing transitive dependencies" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-package-extensions",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "pnpm": {
		    "packageExtensions": {
		      "is-odd@3": {
		        "dependencies": { "kind-of": "6.0.3" }
		      }
		    }
		  }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/kind-of@6.0.3
	run grep 'kind-of: 6.0.3' aube-lock.yaml
	assert_success
}

@test "yarn-style resolutions are honored" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "resolutions": { "is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "top-level overrides win over pnpm.overrides and resolutions" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "resolutions": { "is-number": "3.0.0" },
		  "pnpm": { "overrides": { "is-number": "6.0.0" } },
		  "overrides": { "is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "overrides are written to aube-lock.yaml" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	run grep -A1 '^overrides:' aube-lock.yaml
	assert_success
	assert_output --partial 'is-number'
	assert_output --partial '7.0.0'
}

@test "override applies to the real package behind an npm: alias direct dep" {
	# The user declares a dependency under an alias name (`alias-num`)
	# whose real package is `is-number`. The override targets the real
	# name. Without the post-alias override re-check, the resolver would
	# silently ignore the override on the aliased copy.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": {
		    "alias-num": "npm:is-number@^6.0.0",
		    "is-odd": "3.0.1"
		  },
		  "overrides": { "is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
	# Both the aliased direct dep and is-odd's transitive dep must
	# resolve to the same overridden version — no 6.0.0 leak.
	run test -d node_modules/.aube/is-number@6.0.0
	assert_failure
}

@test "override value can itself be an npm: alias" {
	# Override the bare name `is-number` to a `npm:is-number@7.0.0`
	# alias spec. The resolver should rewrite the range, run the alias
	# handler, and land on is-number@7.0.0.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-number": "npm:is-number@7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "pnpm parent>child selector overrides only under the named parent" {
	# is-odd@3.0.1 -> is-number@^6.0.0. Pin it via the `is-odd>is-number`
	# parent-chain selector. Bare-name rule would do the same thing here;
	# the value of this test is that we exercise the ancestor-match path.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-odd>is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
	run test -d node_modules/.aube/is-number@6.0.0
	assert_failure
}

@test "parent>child selector does not override when the parent name doesn't match" {
	# Pin only when is-number is a child of `nonexistent`. is-odd's
	# is-number should therefore resolve normally to 6.0.0.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "nonexistent>is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@6.0.0
	run test -d node_modules/.aube/is-number@7.0.0
	assert_failure
}

@test "yarn **/foo wildcard selector overrides like a bare name" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "resolutions": { "**/is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "yarn parent/child selector overrides under the named parent" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "resolutions": { "is-odd/is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "version-qualified target selector applies when the range overlaps" {
	# is-odd@3.0.1 asks for is-number ^6, and the selector `is-number@<7`
	# matches a range whose lower bound is in (<7). Pin to 7.0.0.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-number@<7": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "version-qualified target selector does not apply when the range is outside" {
	# The requested range `^6.0.0` does not overlap `>=8`, so the
	# override should be skipped and is-number resolves to 6.0.0.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-number@>=8": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@6.0.0
	run test -d node_modules/.aube/is-number@7.0.0
	assert_failure
}

@test "parent version constraint filters the override to matching parents" {
	# is-odd@3.0.1 matches ^3, so the selector fires; 7.0.0 wins.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-odd@^3>is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "parent version constraint skips the override when the parent is outside" {
	# is-odd@3.0.1 does NOT match ^2, so the selector should not fire.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-odd@^2>is-number": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@6.0.0
}

@test "override value \"-\" removes a transitive dep entirely" {
	# pnpm's removal marker: `"-"` drops the dep edge from the graph.
	# is-odd@3.0.1 normally pulls is-number@^6.0.0 — removing it means
	# no version of is-number should land in the virtual store.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "pnpm": { "overrides": { "is-number": "-" } }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-odd@3.0.1
	run test -d node_modules/.aube/is-number@6.0.0
	assert_failure
	# No is-number snapshot should land in the lockfile. The override
	# value here is `-`, so the `overrides:` block won't carry an
	# `is-number@` substring either.
	run grep 'is-number@' aube-lock.yaml
	assert_failure
}

@test "parent>child \"-\" selector removes only the matching edge" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-odd>is-number": "-" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-odd@3.0.1
	run test -d node_modules/.aube/is-number@6.0.0
	assert_failure
}

@test "override value \"-\" removes a direct dep" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-odd": "3.0.1",
		    "is-number": "6.0.0"
		  },
		  "overrides": { "is-number": "-" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-odd@3.0.1
	run test -d node_modules/.aube/is-number@6.0.0
	assert_failure
	run test -L node_modules/is-number
	assert_failure
}

@test "pnpm-workspace.yaml overrides survive --frozen-lockfile" {
	# pnpm v10 moved overrides to pnpm-workspace.yaml. The resolver has
	# always read them, but the lockfile drift check used to compare
	# against `package.json`'s overrides only — so the second run (with
	# --frozen-lockfile) rejected the lockfile with "manifest removes".
	# Regression for #174.
	cat >package.json <<-'EOF'
		{
		  "name": "test-ws-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-workspace.yaml <<-'EOF'
		packages: []
		overrides:
		  is-number: 7.0.0
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0

	# Same project, --frozen-lockfile. Must NOT claim drift.
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "frozen-lockfile accepts version-keyed override that rewrites importer specifier" {
	# discussion #352: a name+range override (`is-number@<7.0.0` →
	# `7.0.0`) rewrites the lockfile's importer `specifier:` to the
	# override target. The frozen check has to apply the same override
	# to the manifest spec before comparing — otherwise every
	# subsequent `aube install --frozen-lockfile` reads stale even
	# though the lockfile is in sync.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-number": "^6.0.0" },
		  "overrides": { "is-number@<7.0.0": "7.0.0" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0

	# Lockfile is in sync; frozen-lockfile must not report stale.
	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}

@test "changing overrides re-resolves the lockfile on next install" {
	# First install with no override.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/.aube/is-number@6.0.0

	# Now add an override and re-install. Drift detection on the
	# `overrides` block should kick the resolver back on.
	cat >package.json <<-'EOF'
		{
		  "name": "test-overrides",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "overrides": { "is-number": "7.0.0" }
		}
	EOF
	run aube install
	assert_success
	assert_dir_exists node_modules/.aube/is-number@7.0.0
}
