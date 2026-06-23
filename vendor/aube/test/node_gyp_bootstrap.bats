#!/usr/bin/env bats
#
# Tests for aube's on-demand `node-gyp` bootstrap. When a dep
# lifecycle script needs `node-gyp` and the ambient PATH has none,
# aube installs a pinned copy under
# `$XDG_CACHE_HOME/aube/tools/node-gyp/<bucket>/` and prepends its
# `.bin` to the script's PATH. The offline Verdaccio fixture ships
# node-gyp and its transitive deps under `test/registry/storage/`
# so this test runs without network access.

setup() {
	load 'test_helper/common_setup'
	_common_setup

	# Scrub any inherited `node-gyp` off PATH so the only way the
	# lifecycle script below can resolve it is through the bootstrap.
	# Verdaccio is reached via AUBE_TEST_REGISTRY; _common_setup
	# already wrote it into .npmrc, which propagates to the nested
	# `aube install` the bootstrap spawns.
	local sanitized=""
	local entry
	while IFS= read -r entry; do
		if [ -z "$entry" ]; then
			continue
		fi
		if [ -x "$entry/node-gyp" ] || [ -f "$entry/node-gyp" ]; then
			continue
		fi
		sanitized="${sanitized}${sanitized:+:}${entry}"
	done < <(printf '%s\n' "$PATH" | tr ':' '\n')
	export PATH="$sanitized"
	if command -v node-gyp >/dev/null 2>&1; then
		skip "node-gyp still on PATH after scrub ($(command -v node-gyp)); cannot exercise bootstrap"
	fi
}

teardown() {
	_common_teardown
}

@test "bootstrap installs node-gyp when missing from PATH" {
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
	# Minimal project that would run an `install` lifecycle script.
	# We don't need the script to actually succeed — we just need a
	# dep whose install phase triggers `run_dep_lifecycle_scripts`,
	# which is gated on `has_dep_lifecycle_work`. Use the existing
	# `aube-test-binding-gyp` fixture: it has a binding.gyp and no
	# install/preinstall, so aube's `default_install_script`
	# fallback runs `node-gyp rebuild`. The rebuild will fail (no C
	# toolchain, no real Python wiring to this fixture), but by then
	# the bootstrap has already run — which is what we're asserting.
	cat >package.json <<'JSON'
{
  "name": "binding-gyp-bootstrap-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-binding-gyp": "^1.0.0"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-binding-gyp": true
    }
  }
}
JSON
	# Don't `assert_success` — the `node-gyp rebuild` subprocess may
	# fail without a real native toolchain. We only care that the
	# bootstrap landed node-gyp into the aube cache.
	run aube install
	assert_dir_exists "$XDG_CACHE_HOME/aube/tools/node-gyp/v12/node_modules/.bin"
	assert_file_exists "$XDG_CACHE_HOME/aube/tools/node-gyp/v12/node_modules/.bin/node-gyp"
}

@test "aube test adds bootstrapped node-gyp to PATH" {
	# Ported from pnpm/test/install/lifecycleScripts.ts:128
	# ('node-gyp is in the PATH'). The setup scrubbed ambient node-gyp,
	# so success means aube supplied its cached tool shim to the script.
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
	cat >package.json <<'JSON'
{
  "name": "node-gyp-path-test",
  "version": "1.0.0",
  "scripts": {
    "test": "node-gyp --help >/dev/null && node -e 'require(\"fs\").writeFileSync(\"node-gyp-ok\", \"ok\")'"
  }
}
JSON

	run aube test
	assert_success
	assert_file_exists node-gyp-ok
}

@test "aube test does not bootstrap node-gyp for unrelated scripts" {
	# Regression for PR review feedback: a cold node-gyp cache plus an
	# unreachable registry must not break scripts that don't call node-gyp.
	# A cheap lazy shim may be written, but the real node-gyp install must
	# not run unless that shim is executed.
	cat >.npmrc <<'EOF'
registry=http://127.0.0.1:9/
EOF
	cat >package.json <<'JSON'
{
  "name": "node-gyp-unrelated-script-test",
  "version": "1.0.0",
  "scripts": {
    "test": "node -e 'require(\"fs\").writeFileSync(\"plain-ok\", \"ok\")'"
  }
}
JSON

	run aube test
	assert_success
	assert_file_exists plain-ok
	assert_dir_not_exists "$XDG_CACHE_HOME/aube/tools/node-gyp/v12"
}

@test "aube test exposes node-gyp to indirect script subprocesses" {
	# pnpm adds its own node-gyp-bin directory to every script PATH. Aube
	# mirrors that with a lazy shim so `node build.js` can spawn node-gyp
	# without sniffing the package.json script text up front.
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi
	cat >package.json <<'JSON'
{
  "name": "node-gyp-indirect-path-test",
  "version": "1.0.0",
  "scripts": {
    "test": "node build.js"
  }
}
JSON
	cat >build.js <<'JS'
const cp = require('child_process');
const fs = require('fs');
const result = cp.spawnSync('node-gyp', ['--help'], {stdio: 'ignore'});
if (result.status === 0) {
  fs.writeFileSync('node-gyp-indirect-ok', 'ok');
}
process.exit(result.status ?? 1);
JS

	run aube test
	assert_success
	assert_file_exists node-gyp-indirect-ok
}

@test "aube run passes workspace root to lazy node-gyp bootstrap" {
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - packages/*
YAML
	mkdir -p packages/app
	cat >packages/app/package.json <<'JSON'
{
  "name": "node-gyp-workspace-root-test",
  "version": "1.0.0",
  "scripts": {
    "test": "node -e 'require(\"fs\").writeFileSync(\"node-gyp-project-dir\", process.env.AUBE_NODE_GYP_PROJECT_DIR)'"
  }
}
JSON

	run aube --filter node-gyp-workspace-root-test test --no-install
	assert_success
	assert_file_contains packages/app/node-gyp-project-dir "$TEST_TEMP_DIR"
	refute grep -q 'packages/app' packages/app/node-gyp-project-dir
}

@test "aube test uses project node-gyp bin without bootstrapping" {
	# Regression for PR review feedback: if the project already installed
	# node-gyp, a cold aube tool cache and unreachable registry must not
	# block the script.
	cat >.npmrc <<'EOF'
registry=http://127.0.0.1:9/
EOF
	mkdir -p node_modules/.bin
	cat >node_modules/.bin/node-gyp <<SHIM
#!/usr/bin/env bash
printf 'project-node-gyp\n' > "$TEST_TEMP_DIR/project-node-gyp"
SHIM
	chmod +x node_modules/.bin/node-gyp
	cat >package.json <<'JSON'
{
  "name": "node-gyp-project-bin-test",
  "version": "1.0.0",
  "scripts": {
    "test": "node-gyp --help"
  }
}
JSON

	run aube test --no-install
	assert_success
	assert_file_exists project-node-gyp
	# No *bootstrap* (the `v12/` bucket) — the project's own node-gyp was
	# used. A cheap network-free lazy shim under `lazy-bin/` (backing
	# `npm_config_node_gyp`) may exist; it never triggers an install.
	assert_dir_not_exists "$XDG_CACHE_HOME/aube/tools/node-gyp/v12"
}
