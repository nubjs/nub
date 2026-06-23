#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube import --help" {
	run aube import --help
	assert_success
	assert_output --partial "Convert a supported lockfile into aube-lock.yaml"
}

@test "aube import from package-lock.json writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from package-lock.json"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
	run grep -c "@sindresorhus/is@5.6.0" aube-lock.yaml
	assert_success
}

@test "aube import from yarn.lock writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from yarn.lock"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
}

@test "aube import from yarn berry (v2+) yarn.lock writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/yarn.lock" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from yarn.lock"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
	run grep -c "@sindresorhus/is@5.6.0" aube-lock.yaml
	assert_success
}

@test "aube import from npm-shrinkwrap.json writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/npm-shrinkwrap.json" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from npm-shrinkwrap.json"

	assert_file_exists aube-lock.yaml
}

@test "aube import from bun.lock writes aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-bun/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun/bun.lock" .

	run aube import
	assert_success
	assert_output --partial "Imported 4 packages from bun.lock"

	assert_file_exists aube-lock.yaml
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
}

@test "aube install reads bun.lock when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-bun/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun/bun.lock" .

	run aube -v install
	assert_success
	assert_output --partial "bun.lock: 4 packages"

	assert_dir_exists node_modules/is-odd
	assert_dir_exists "node_modules/@sindresorhus/is"
}

@test "aube install applies yarn berry patch protocol" {
	cp -R "$PROJECT_ROOT/fixtures/import-yarn-patch/." .

	run aube install --ignore-scripts
	assert_success

	run node -e 'const isNumber = require("is-number"); if (isNumber(41) || !isNumber(42)) process.exit(1)'
	assert_success
}

@test "aube install supports yarn berry portal and exec protocols" {
	cp -R "$PROJECT_ROOT/fixtures/import-yarn-portal-exec/." .

	run aube install
	assert_success

	run node -e 'if (require("portal-pkg") !== "portal ok + exec ok") process.exit(1); if (require("exec-pkg") !== "exec ok") process.exit(1)'
	assert_success
}

@test "aube install supports yarn berry esm exec generators" {
	cat >package.json <<'EOF'
{"name":"root","version":"1.0.0","dependencies":{"esm-exec-pkg":"exec:./scripts/generate-exec.mjs"}}
EOF
	mkdir -p scripts
	cat >scripts/generate-exec.mjs <<'EOF'
fs.writeFileSync(path.join(execEnv.buildDir, 'package.json'), JSON.stringify({
  name: 'esm-exec-pkg',
  version: '1.0.0',
  main: 'index.js'
}));
fs.writeFileSync(path.join(execEnv.buildDir, 'index.js'), "module.exports = 'esm exec ok';\n");
EOF

	run aube install
	assert_success

	run node -e 'if (require("esm-exec-pkg") !== "esm exec ok") process.exit(1)'
	assert_success
}

@test "aube install fresh-resolves yarn berry portal transitive exec protocol" {
	cp -R "$PROJECT_ROOT/fixtures/import-yarn-portal-exec/." .
	rm yarn.lock

	run aube install
	assert_success

	run node -e 'if (require("portal-pkg") !== "portal ok + exec ok") process.exit(1); if (require("exec-pkg") !== "exec ok") process.exit(1)'
	assert_success
	run grep -F "is-number@7.0.0:" aube-lock.yaml
	assert_success
	run grep -F "version: exec:scripts/generate-exec.js" aube-lock.yaml
	assert_success
	run grep -A 4 -F "portal-pkg@portal:./packages/portal:" aube-lock.yaml
	assert_output --partial "exec-pkg: exec:scripts/generate-exec.js"
	run grep -F "0.0.0" aube-lock.yaml
	assert_failure
}

@test "aube install --ignore-scripts does not run yarn berry exec generator during fresh resolve" {
	cp -R "$PROJECT_ROOT/fixtures/import-yarn-portal-exec/." .
	rm yarn.lock

	run aube install --ignore-scripts
	assert_failure
	assert_output --partial "scripts are disabled"
}

@test "aube install from bun.lock links workspace deps to their package dirs" {
	cat >package.json <<'EOF'
{"name":"root","version":"1.0.0","workspaces":["packages/*"]}
EOF
	mkdir -p packages/app packages/lib
	cat >packages/app/package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"lib":"workspace:*"}}
EOF
	cat >packages/lib/package.json <<'EOF'
{"name":"lib","version":"0.0.0"}
EOF
	cat >bun.lock <<'EOF'
{
  "lockfileVersion": 1,
  "workspaces": {
    "": { "name": "root" },
    "packages/app": {
      "name": "app",
      "version": "0.0.0",
      "dependencies": { "lib": "workspace:*" }
    },
    "packages/lib": {
      "name": "lib",
      "version": "0.0.0"
    }
  },
  "packages": {
    "app": ["app@workspace:packages/app"],
    "lib": ["lib@workspace:packages/lib"]
  }
}
EOF

	run aube install --frozen-lockfile
	assert_success

	actual="$(realpath packages/app/node_modules/lib)"
	expected="$(realpath packages/lib)"
	[[ "$actual" == "$expected" ]] || fail "lib link points to $actual, expected $expected"
}

@test "aube install applies peer-context suffixes when importing from bun.lock" {
	# Regression for the bun.lock importer skipping
	# `apply_peer_contexts`. Without the pass, `fdir` lands in the
	# virtual store as `fdir@6.5.0/` with no peer-qualified sibling
	# link, so any consumer that runs from inside the isolated dir
	# can't find `picomatch` and falls through to whatever copy is
	# hoisted in `.aube/node_modules/`.
	cp "$PROJECT_ROOT/fixtures/import-bun-peer/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun-peer/bun.lock" .

	run aube install
	assert_success

	# Resolve the peer-qualified virtual-store directory via a glob
	# so the test fails loudly if zero or multiple entries match
	# (rather than papering over the breakage with a literal
	# newline embedded in the assert_link_exists path).
	local matches=(node_modules/.aube/fdir@6.5.0_picomatch@4.0.4*)
	[ "${#matches[@]}" -eq 1 ] || fail "expected exactly one peer-qualified fdir dir, got: ${matches[*]}"
	[ -d "${matches[0]}" ] || fail "peer-qualified fdir dir missing: ${matches[0]}"

	# And the sibling peer link is wired inside that directory.
	assert_link_exists "${matches[0]}/node_modules/picomatch"
}

@test "aube install hoists peer-only packages from bun.lock that aren't in workspace deps" {
	# Regression for bun.lock importer pruning peer-only-installed
	# packages before the peer-hoist pass could promote them. The
	# workspace declares only `fdir`, but bun has materialized
	# `picomatch` in `packages:` because fdir peers on it. Before the
	# fix, `filter_graph`'s GC walk ran first and dropped `picomatch`
	# as unreachable from importers; the subsequent
	# `hoist_auto_installed_peers` then had nothing to promote.
	cp "$PROJECT_ROOT/fixtures/import-bun-peer-only-in-packages/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-bun-peer-only-in-packages/bun.lock" .

	run aube install
	assert_success

	# Both packages should be installed and reachable from the root.
	assert_dir_exists node_modules/fdir
	assert_dir_exists node_modules/picomatch

	# fdir's isolated dir should carry the peer-context suffix and
	# wire picomatch as a sibling so fdir can resolve it from its
	# own require() context.
	local matches=(node_modules/.aube/fdir@6.5.0_picomatch@4.0.4*)
	[ "${#matches[@]}" -eq 1 ] || fail "expected exactly one peer-qualified fdir dir, got: ${matches[*]}"
	[ -d "${matches[0]}" ] || fail "peer-qualified fdir dir missing: ${matches[0]}"
	assert_link_exists "${matches[0]}/node_modules/picomatch"
}

@test "aube install smoke installs messy bun.lock fixture and doesn't change lockfile" {
	cp -R "$PROJECT_ROOT/fixtures/import-bun-messy/." .
	cp bun.lock bun.lock.before

	run aube install
	assert_success

	run node -e "try { require('is-odd')('nope') } catch (e) { console.log(e.message) }"
	assert_success
	assert_output "expected a numeric value"

	run cmp -s bun.lock bun.lock.before
	assert_success
}

@test "aube import refuses to overwrite existing aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .
	echo "lockfileVersion: '9.0'" >aube-lock.yaml

	run aube import
	assert_failure
	assert_output --partial "already exists"
}

@test "aube import --force overwrites existing aube-lock.yaml" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .
	echo "stale" >aube-lock.yaml

	run aube import --force
	assert_success
	assert_output --partial "Imported"
	run grep -c "is-odd@3.0.1" aube-lock.yaml
	assert_success
}

@test "aube import errors on bun.lockb (binary format)" {
	cat >package.json <<'EOF'
{"name":"test","version":"1.0.0"}
EOF
	# Write fake binary file
	printf '\x00\x01\x02\x03' >bun.lockb

	run aube import
	assert_failure
	assert_output --partial "bun.lockb"
	assert_output --partial "binary format"
}

@test "aube import errors cleanly when no source lockfile exists" {
	cat >package.json <<'EOF'
{"name":"empty","version":"1.0.0"}
EOF

	run aube import
	assert_failure
	assert_output --partial "no source lockfile found"
}

@test "aube import prefers yarn.lock over package-lock.json when both exist" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube import
	assert_success
	assert_output --partial "from yarn.lock"
}

@test "aube install reads package-lock.json when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube -v install
	assert_success
	assert_output --partial "package-lock.json: 4 packages"

	assert_dir_exists node_modules/is-odd
	assert_dir_exists "node_modules/@sindresorhus/is"
	assert_dir_exists node_modules/kind-of
}

@test "aube install reads yarn.lock when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-yarn/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn/yarn.lock" .

	run aube -v install
	assert_success
	assert_output --partial "yarn.lock: 4 packages"

	assert_dir_exists node_modules/is-odd
}

@test "aube install reads yarn berry yarn.lock and rewrites it as berry" {
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-yarn-berry/yarn.lock" .

	run aube -v install
	assert_success
	assert_output --partial "yarn.lock: 4 packages"

	assert_dir_exists node_modules/is-odd
	assert_dir_exists "node_modules/@sindresorhus/is"
	assert_dir_exists node_modules/kind-of

	# aube should have preserved berry format on write-back rather
	# than silently downgrading to classic (v1).
	run grep -c "^__metadata:" yarn.lock
	assert_success
	assert_file_not_exists aube-lock.yaml
}

@test "aube install reads npm-shrinkwrap.json when no aube-lock.yaml present" {
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/npm-shrinkwrap.json" .

	run aube -v install
	assert_success
	assert_output --partial "npm-shrinkwrap.json: 4 packages"

	assert_dir_exists node_modules/is-odd
}

@test "aube install prefers npm-shrinkwrap.json over package-lock.json when both exist" {
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-shrinkwrap/npm-shrinkwrap.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube -v install
	assert_success
	assert_output --partial "npm-shrinkwrap.json: 4 packages"
	refute_output --partial "package-lock.json: 4 packages"
}

@test "aube install prefers aube-lock.yaml when multiple lockfiles coexist" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	# First import to produce aube-lock.yaml
	run aube import
	assert_success

	# Now install — should report "Lockfile:", not "package-lock.json:"
	run aube -v install
	assert_success
	assert_output --partial "Lockfile: 4 packages"
	refute_output --partial "package-lock.json: 4 packages"
}

@test "aube import --lockfile-only: accepted (parity no-op)" {
	cp "$PROJECT_ROOT/fixtures/import-npm/package.json" .
	cp "$PROJECT_ROOT/fixtures/import-npm/package-lock.json" .

	run aube import --lockfile-only
	assert_success
	assert_output --partial 'Imported'

	assert_file_exists aube-lock.yaml
	# Confirms the no-op semantics: import never linked node_modules
	# regardless of the flag.
	run test -d node_modules
	assert_failure
}
