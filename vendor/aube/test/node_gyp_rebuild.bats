#!/usr/bin/env bats
#
# Tests for aube's implicit `node-gyp rebuild` fallback. When a
# dependency ships a top-level `binding.gyp` but defines neither an
# `install` nor a `preinstall` script, npm/pnpm both default the
# install command to `node-gyp rebuild`. Aube matches that so native
# modules without a prebuilt binary still compile on install.
#
# The fixture package `aube-test-binding-gyp` (under
# `test/registry/storage/`) has a minimal `binding.gyp` and no
# lifecycle scripts. Each test drops a `node-gyp` shim on `PATH` that
# writes a marker file so we can confirm aube invoked it with the
# expected argument, without needing a real C toolchain in CI.

setup() {
	load 'test_helper/common_setup'
	_common_setup

	# Shim directory is prepended to PATH so lifecycle scripts pick
	# it up instead of a real node-gyp. The shim records its argv
	# and cwd so assertions can verify aube invoked `node-gyp
	# rebuild` from the dep's materialized directory.
	mkdir -p "$TEST_TEMP_DIR/shim"
	cat >"$TEST_TEMP_DIR/shim/node-gyp" <<'SHIM'
#!/usr/bin/env bash
marker="${INIT_CWD:-$PWD}/node-gyp.marker"
printf 'argv=%s\n' "$*" >"$marker"
printf 'cwd=%s\n' "$PWD" >>"$marker"
printf 'pkg=%s@%s\n' "$npm_package_name" "$npm_package_version" >>"$marker"
SHIM
	chmod +x "$TEST_TEMP_DIR/shim/node-gyp"
	export PATH="$TEST_TEMP_DIR/shim:$PATH"
}

teardown() {
	_common_teardown
}

@test "binding.gyp dep is skipped by default (not allowlisted)" {
	cat >package.json <<'JSON'
{
  "name": "binding-gyp-default-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-binding-gyp": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success
	assert_file_not_exists node-gyp.marker
}

@test "allowlisted binding.gyp dep runs implicit node-gyp rebuild" {
	cat >package.json <<'JSON'
{
  "name": "binding-gyp-rebuild-test",
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
	run aube install
	assert_success
	assert_file_exists node-gyp.marker
	run cat node-gyp.marker
	assert_line "argv=rebuild"
	assert_line "pkg=aube-test-binding-gyp@1.0.0"
	# Implicit rebuild must run from inside the dep's materialized
	# package directory — i.e. the virtual-store leaf whose path
	# ends in `/node_modules/aube-test-binding-gyp` — *not* the
	# project root, or node-gyp would look at the wrong
	# `binding.gyp`.
	assert_line --regexp "^cwd=.*/node_modules/aube-test-binding-gyp$"
	refute_line "cwd=$PWD"
}

@test "side-effects-cache restores allowlisted node-gyp build output" {
	cat >"$TEST_TEMP_DIR/shim/node-gyp" <<'SHIM'
#!/usr/bin/env bash
count="${INIT_CWD:-$PWD}/node-gyp-count"
n=0
if [ -f "$count" ]; then
  n="$(cat "$count")"
fi
printf '%s\n' "$((n + 1))" >"$count"
printf 'built\n' >"$PWD/side-effects-cache-output.txt"
SHIM
	chmod +x "$TEST_TEMP_DIR/shim/node-gyp"

	cat >package.json <<'JSON'
{
  "name": "binding-gyp-side-effects-cache-test",
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
	run aube install
	assert_success
	assert_file_exists node-gyp-count
	run cat node-gyp-count
	assert_output "1"
	assert_file_exists node_modules/aube-test-binding-gyp/side-effects-cache-output.txt

	rm -rf node_modules "$HOME/.cache/aube/virtual-store"
	run aube install
	assert_success
	run cat node-gyp-count
	assert_output "1"
	assert_file_exists node_modules/aube-test-binding-gyp/side-effects-cache-output.txt
}

@test "rebuild refreshes side-effects-cache output" {
	cat >"$TEST_TEMP_DIR/shim/node-gyp" <<'SHIM'
#!/usr/bin/env bash
count="${INIT_CWD:-$PWD}/node-gyp-count"
n=0
if [ -f "$count" ]; then
  n="$(cat "$count")"
fi
n="$((n + 1))"
printf '%s\n' "$n" >"$count"
printf 'built-%s\n' "$n" >"$PWD/side-effects-cache-output.txt"
SHIM
	chmod +x "$TEST_TEMP_DIR/shim/node-gyp"

	cat >package.json <<'JSON'
{
  "name": "binding-gyp-rebuild-side-effects-cache-test",
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
	run aube install
	assert_success
	run cat node-gyp-count
	assert_output "1"
	run cat node_modules/aube-test-binding-gyp/side-effects-cache-output.txt
	assert_output "built-1"

	run aube rebuild
	assert_success
	run cat node-gyp-count
	assert_output "2"
	run cat node_modules/aube-test-binding-gyp/side-effects-cache-output.txt
	assert_output "built-2"

	rm -rf node_modules "$HOME/.cache/aube/virtual-store"
	run aube install
	assert_success
	run cat node-gyp-count
	assert_output "2"
	run cat node_modules/aube-test-binding-gyp/side-effects-cache-output.txt
	assert_output "built-2"
}

@test "ignored-builds lists a binding.gyp dep that wasn't allowlisted" {
	cat >package.json <<'JSON'
{
  "name": "binding-gyp-ignored-builds-test",
  "version": "1.0.0",
  "dependencies": {
    "aube-test-binding-gyp": "^1.0.0"
  }
}
JSON
	run aube install
	assert_success
	run aube ignored-builds
	assert_success
	assert_output --partial "aube-test-binding-gyp"
}

@test "bootstrap is skipped when node-gyp is already on PATH" {
	# The shim installed in `setup()` is a `node-gyp` on PATH, so
	# `node_gyp_bootstrap::ensure()` must return `None` and leave the
	# cache untouched. If we ever regress to bootstrapping
	# unconditionally this test will start hitting the real npm
	# registry and fail in the hermetic CI environment.
	cat >package.json <<'JSON'
{
  "name": "binding-gyp-bootstrap-skip-test",
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
	run aube install
	assert_success
	# The shim wrote the marker, proving the on-PATH copy was used.
	assert_file_exists node-gyp.marker
	# And the aube cache must NOT have a *bootstrapped* node-gyp (the
	# `v12/` bucket). A cheap network-free lazy shim (`lazy-bin/`) may
	# exist — it backs `npm_config_node_gyp` and never bootstraps unless
	# something executes it — so assert specifically on the bootstrap dir.
	assert_dir_not_exists "$XDG_CACHE_HOME/aube/tools/node-gyp/v12"
}

@test "--ignore-scripts suppresses the implicit node-gyp rebuild" {
	cat >package.json <<'JSON'
{
  "name": "binding-gyp-ignore-scripts-test",
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
	run aube install --ignore-scripts
	assert_success
	assert_file_not_exists node-gyp.marker
}
