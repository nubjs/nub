#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	if [ -n "${SPLIT_REGISTRY_PID:-}" ]; then
		kill "$SPLIT_REGISTRY_PID" 2>/dev/null || true
		wait "$SPLIT_REGISTRY_PID" 2>/dev/null || true
	fi
	_common_teardown
}

# `lockfile=false` skips reading and writing aube-lock.yaml entirely
# (npm's --no-package-lock equivalent).

@test "lockfile=false skips writing aube-lock.yaml" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-no-lockfile",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		lockfile=false
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	run test -e aube-lock.yaml
	assert_failure
	run test -e pnpm-lock.yaml
	assert_failure
	# node_modules should still be linked even without the lockfile.
	assert_dir_exists node_modules/is-odd
}

@test "lockfile=false does not read an existing aube-lock.yaml for drift" {
	# Seed an obviously stale lockfile. If the install reads it, it
	# would drift-error under the default Prefer mode; with lockfile=false
	# the install resolves from scratch and ignores the file.
	cat >package.json <<-'EOF'
		{
		  "name": "test-no-lockfile-read",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >aube-lock.yaml <<-'EOF'
		# intentionally malformed
		this is not a valid lockfile
	EOF
	cat >>.npmrc <<-'EOF'

		lockfile=false
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	# The lockfile on disk is left alone — we don't overwrite it either.
	run cat aube-lock.yaml
	assert_output --partial 'not a valid lockfile'
}

@test "lockfile=false + --lockfile-only errors with a clear message" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-conflict",
		  "version": "1.0.0"
		}
	EOF
	cat >>.npmrc <<-'EOF'

		lockfile=false
	EOF
	run aube install --lockfile-only
	assert_failure
	assert_output --partial 'incompatible with lockfile=false'
}

# `--frozen-lockfile` ("fail hard if the lockfile doesn't match") makes
# no sense without a lockfile. Without this check the install falls
# through to the catch-all Err(NotFound) arm and errors with the
# generic "no lockfile found and --frozen-lockfile is set" message,
# which hides the real conflict from the user.
@test "lockfile=false + --frozen-lockfile errors with a clear message" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-conflict-frozen",
		  "version": "1.0.0"
		}
	EOF
	cat >>.npmrc <<-'EOF'

		lockfile=false
	EOF
	run aube --frozen-lockfile install
	assert_failure
	assert_output --partial 'incompatible with lockfile=false'
	refute_output --partial 'no lockfile found'
}

# `lockfileIncludeTarballUrl=true` records each registry package's full
# tarball URL in the lockfile's `resolution.tarball:` field.

@test "lockfileIncludeTarballUrl=true embeds tarball URLs in aube-lock.yaml" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-tarball-url",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		lockfile-include-tarball-url=true
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	# The tarball URL for is-odd@3.0.1 on the test registry must be in
	# the lockfile's resolution block. We assert the trailing
	# `/is-odd/-/is-odd-3.0.1.tgz` so the check works regardless of
	# whether the tests run against the local Verdaccio mirror or the
	# real registry.
	run grep -F "is-odd-3.0.1.tgz" aube-lock.yaml
	assert_success
	# And the `settings:` header should reflect the opt-in so the next
	# install round-trips the tarball URLs without re-reading .npmrc.
	run grep "lockfileIncludeTarballUrl: true" aube-lock.yaml
	assert_success
}

@test "lockfile tarball URLs are verified against registry metadata before fetch" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-tarball-url-mismatch",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		lockfile-include-tarball-url=true
	EOF
	run aube install --no-frozen-lockfile
	assert_success

	perl -0pi -e 's#tarball: .*/is-odd/-/is-odd-3\.0\.1\.tgz#tarball: https://example.com/not-is-odd.tgz#' aube-lock.yaml
	rm -rf node_modules "$XDG_DATA_HOME/aube/store"

	run aube install --frozen-lockfile
	assert_failure
	assert_output --partial "ERR_AUBE_TARBALL_URL_MISMATCH"
	assert_output --partial "not match registry metadata"
}

@test "lockfile tarball URLs are verified against packument metadata" {
	mkdir -p package
	cat >package/package.json <<-'EOF'
		{
		  "name": "split-pkg",
		  "version": "1.0.0"
		}
	EOF
	tar -czf split-pkg-1.0.0.tgz package
	integrity="$(node -e "const crypto = require('node:crypto'); const fs = require('node:fs'); console.log('sha512-' + crypto.createHash('sha512').update(fs.readFileSync('split-pkg-1.0.0.tgz')).digest('base64'))")"

	cat >split-registry.mjs <<'NODE'
import http from 'node:http';
import fs from 'node:fs';

const integrity = process.env.SPLIT_PKG_INTEGRITY;
const tarball = fs.readFileSync('split-pkg-1.0.0.tgz');
const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/split-pkg') {
    const tarballUrl = `http://${req.headers.host}/split-pkg/-/split-pkg-1.0.0.tgz`;
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({
      name: 'split-pkg',
      'dist-tags': { latest: '1.0.0' },
      versions: {
        '1.0.0': {
          name: 'split-pkg',
          version: '1.0.0',
          dist: { tarball: tarballUrl, integrity }
        }
      },
      time: { '1.0.0': '2024-01-01T00:00:00.000Z' }
    }));
    return;
  }
  if (req.method === 'GET' && req.url === '/split-pkg/-/split-pkg-1.0.0.tgz') {
    res.setHeader('content-type', 'application/octet-stream');
    res.end(tarball);
    return;
  }
  res.statusCode = 404;
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync('split-registry-port', String(server.address().port));
});
NODE
	SPLIT_PKG_INTEGRITY="$integrity" node split-registry.mjs &
	SPLIT_REGISTRY_PID=$!
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		[ -f split-registry-port ] && break
		sleep 0.1
	done
	registry="http://127.0.0.1:$(cat split-registry-port)"

	cat >package.json <<-'EOF'
		{
		  "name": "test-packument-tarball-url",
		  "version": "1.0.0",
		  "dependencies": { "split-pkg": "1.0.0" }
		}
	EOF
	cat >.npmrc <<-EOF
		registry=$registry
		lockfile-include-tarball-url=true
	EOF

	run aube install --no-frozen-lockfile
	assert_success

	rm -rf node_modules "$XDG_DATA_HOME/aube/store" "$XDG_CACHE_HOME/aube/packuments-v1"

	run aube install --frozen-lockfile
	kill "$SPLIT_REGISTRY_PID"
	wait "$SPLIT_REGISTRY_PID" 2>/dev/null || true
	unset SPLIT_REGISTRY_PID
	assert_success
}

@test "lockfileIncludeTarballUrl=false (default) omits tarball URLs" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-tarball-url-off",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	run grep -F "is-odd-3.0.1.tgz" aube-lock.yaml
	assert_failure
}

# `modulesDir` is fully wired: the linker writes into the configured
# outer directory and every command that touches the project-level
# tree (`bin`, `root`, `prune`, `clean`, `run`, `exec`, …) honors it.
# `virtualStoreDir` is wired too: the linker, engines check, fetch
# fast path, orphan sweep, `aube patch`, and `aube unlink` all resolve
# the configured path (or derive `<modulesDir>/.aube` when unset), so
# an override relocates the inner `.aube/<dep>/node_modules/` tree
# without leaving behind a stale one at the default location.

@test "modulesDir=custom installs into the configured directory" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-modules-dir",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		modules-dir=custom_modules
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	# Installed into the configured outer directory, not node_modules
	assert_dir_exists custom_modules/is-odd
	assert_dir_exists custom_modules/.aube
	run test -e node_modules
	assert_failure
	# Inner virtual-store tree keeps the literal `node_modules` name
	# Node's resolver expects when walking up from inside a package
	run ls custom_modules/.aube
	assert_success
	# `aube root` / `aube bin` print the configured paths
	run aube root
	assert_success
	assert_output --partial "custom_modules"
	refute_output --partial "/node_modules"
	run aube bin
	assert_success
	assert_output --partial "custom_modules/.bin"
}

@test "virtualStoreDir=custom relocates the inner .aube tree" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-virtual-store-dir",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	# Relative path resolves against the project root, matching
	# pnpm's behavior.
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=node_modules/.custom-vs
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	# Top-level symlink still lands under node_modules/
	assert_dir_exists node_modules/is-odd
	# Inner virtual store lives at the configured path, not the default
	assert_dir_exists node_modules/.custom-vs
	run test -e node_modules/.aube
	assert_failure
	# The is-odd dep's materialized copy is reachable through the
	# configured path
	run ls node_modules/.custom-vs
	assert_success
	assert_output --partial 'is-odd'
}

@test "virtualStoreDir=non-dotfile name under modulesDir survives the stale sweep" {
	# Regression: link_all's stale-entry sweep removes any child of
	# `modulesDir` that doesn't start with '.', isn't in root deps,
	# and isn't a scope prefix. With a default `.aube` the dotfile
	# check skips it, but a user-configured `vstore` name gets wiped
	# immediately after mkdirp. Now that the sweep honors the
	# resolved aube_dir leaf, this install should work and the
	# virtual store should persist at the configured name.
	cat >package.json <<-'EOF'
		{
		  "name": "test-virtual-store-dir-non-dot",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=node_modules/vstore
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
	assert_dir_exists node_modules/vstore
	run ls node_modules/vstore
	assert_success
	assert_output --partial 'is-odd'
	run test -e node_modules/.aube
	assert_failure
}

@test "virtualStoreDir outside modulesDir places .aube separately" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-virtual-store-dir-outside",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	# Virtual store at a sibling directory instead of under node_modules/
	cat >>.npmrc <<-'EOF'

		virtual-store-dir=.vstore-out
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
	assert_dir_exists .vstore-out
	run test -e node_modules/.aube
	assert_failure
	run ls .vstore-out
	assert_success
	assert_output --partial 'is-odd'
}

@test "modulesDir=node_modules is accepted (default value round-trips)" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-modules-dir-default",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		modules-dir=node_modules
		virtual-store-dir=node_modules/.aube
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
}
