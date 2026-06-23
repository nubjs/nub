#!/usr/bin/env bats
# Node runtime switching: version sources (.nvmrc / .node-version /
# devEngines.runtime), install discovery (aube dir + mise dir), onFail
# policies, and `aube runtime` CLI surface. Hermetic — no network:
# runtimes are fabricated on disk (a shell script that prints its
# version), and download paths live in runtime_download.bats behind a
# local mirror.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Fabricate an installed Node at <root>/<version>/bin/node that just
# prints its version. Matches the native unix layout discovery expects.
_fab_node() {
	local root="$1" version="$2"
	mkdir -p "$root/$version/bin"
	printf '#!/bin/sh\necho "v%s"\n' "$version" >"$root/$version/bin/node"
	chmod +x "$root/$version/bin/node"
}

_aube_runtime_dir() {
	echo "$XDG_DATA_HOME/aube/nodejs"
}

_mise_node_dir() {
	echo "$XDG_DATA_HOME/mise/installs/node"
}

_basic_project() {
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" }
		}
	JSON
}

@test "nvmrc switches scripts to an aube-managed install" {
	_basic_project
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	echo "0.99" >.nvmrc
	run aube install
	assert_success
	run aubr which-node
	assert_success
	assert_output --partial "v0.99.1"
}

@test "highest satisfying installed version wins" {
	_basic_project
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	_fab_node "$(_aube_runtime_dir)" "0.99.7"
	echo "0.99" >.nvmrc
	run aubr which-node
	assert_success
	assert_output --partial "v0.99.7"
}

@test "node-version file beats nvmrc" {
	_basic_project
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	_fab_node "$(_aube_runtime_dir)" "0.98.2"
	echo "0.99" >.nvmrc
	echo "0.98" >.node-version
	run aubr which-node
	assert_success
	assert_output --partial "v0.98.2"
}

@test "devEngines.runtime beats version files" {
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	_fab_node "$(_aube_runtime_dir)" "0.98.2"
	echo "0.98" >.nvmrc
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" },
		  "devEngines": {
		    "runtime": { "name": "node", "version": "^0.99.0", "onFail": "download" }
		  }
		}
	JSON
	run aubr which-node
	assert_success
	assert_output --partial "v0.99.1"
}

@test "mise installs are discovered and reused" {
	_basic_project
	_fab_node "$(_mise_node_dir)" "0.97.3"
	echo "0.97" >.nvmrc
	run aubr which-node
	assert_success
	assert_output --partial "v0.97.3"
}

@test "incomplete mise installs are skipped" {
	_basic_project
	_fab_node "$(_mise_node_dir)" "0.97.9"
	touch "$(_mise_node_dir)/0.97.9/incomplete"
	_fab_node "$(_mise_node_dir)" "0.97.3"
	echo "0.97" >.nvmrc
	run aubr which-node
	assert_success
	assert_output --partial "v0.97.3"
}

@test "mise alias symlinks are not treated as installs" {
	_basic_project
	_fab_node "$(_mise_node_dir)" "0.97.3"
	ln -s "$(_mise_node_dir)/0.97.3" "$(_mise_node_dir)/latest"
	ln -s "$(_mise_node_dir)/0.97.3" "$(_mise_node_dir)/0.97"
	echo "0.97" >.nvmrc
	run aube runtime list
	assert_success
	# The real install shows up once; the symlinked aliases do not
	# produce duplicate entries.
	[ "$(echo "$output" | grep -c '0.97.3 (mise)')" -eq 1 ]
}

@test "aube version dir beats mise copy of the same version" {
	_basic_project
	_fab_node "$(_mise_node_dir)" "0.97.3"
	_fab_node "$(_aube_runtime_dir)" "0.97.3"
	echo "0.97" >.nvmrc
	run aube runtime list
	assert_success
	assert_output --partial "0.97.3 (aube)"
}

@test "devEngines onFail=error fails when unsatisfied" {
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" },
		  "devEngines": {
		    "runtime": { "name": "node", "version": ">=999", "onFail": "error" }
		  }
		}
	JSON
	run aubr which-node
	assert_failure
	assert_output --partial "requires Node.js >=999"
}

@test "devEngines onFail=warn keeps the ambient node" {
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" },
		  "devEngines": {
		    "runtime": { "name": "node", "version": ">=999", "onFail": "warn" }
		  }
		}
	JSON
	run aubr which-node
	assert_success
	# Ambient node answered — its version is whatever the host has,
	# but it's definitely not v999.
	refute_output --partial "v999"
}

@test "runtimeOnFail setting overrides version-file download default" {
	_basic_project
	# No 0.96.x exists anywhere; the .nvmrc default policy would try
	# to download. runtimeOnFail=error must fail before any network.
	echo "0.96" >.nvmrc
	echo "runtime-on-fail=error" >>.npmrc
	run aubr which-node
	assert_failure
	assert_output --partial "requires Node.js 0.96"
}

@test "aube exec resolves shebang binaries against the switched node" {
	_basic_project
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	echo "0.99" >.nvmrc
	run aube exec --shell-mode -- node --version
	assert_success
	assert_output --partial "v0.99.1"
}

@test "runtime list reports source and provenance" {
	_basic_project
	_fab_node "$(_mise_node_dir)" "0.97.3"
	echo "0.97" >.nvmrc
	run aube runtime list
	assert_success
	assert_output --partial "node 0.97.3"
	assert_output --partial "via .nvmrc"
	assert_output --partial "provided by mise"
}

@test "runtime list without a pin reports PATH node" {
	_basic_project
	run aube runtime list
	assert_success
	assert_output --partial "no project pin"
}

@test "doctor reports runtime source and provenance" {
	_basic_project
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	echo "0.99" >.nvmrc
	run aube doctor
	assert_success
	assert_output --partial "node-source"
	assert_output --partial ".nvmrc"
	assert_output --partial "node-requested"
}

@test "runtime set rejects non-node runtimes" {
	_basic_project
	run aube runtime set deno 2.0.0
	assert_failure
	assert_output --partial "only manages the \`node\` runtime"
}

@test "no runtime config leaves PATH untouched" {
	_basic_project
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	run aubr which-node
	assert_success
	refute_output --partial "v0.99.1"
}

@test "pnpm-11 lockfile with a runtime pin installs without fetching node from the registry" {
	# Compat: the synthetic `node` importer dep must be routed into the
	# runtime-pin table, not resolved as an npm package. The pinned
	# version is already installed, so no download happens either.
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" },
		  "devEngines": {
		    "runtime": { "name": "node", "version": "^0.99.0", "onFail": "download" }
		  }
		}
	JSON
	cat >pnpm-lock.yaml <<-'YAML'
		lockfileVersion: '9.0'

		settings:
		  autoInstallPeers: true
		  excludeLinksFromLockfile: false

		importers:

		  .:
		    devDependencies:
		      node:
		        specifier: runtime:^0.99.0
		        version: runtime:0.99.1

		packages:

		  node@runtime:0.99.1:
		    hasBin: true
		    version: 0.99.1
		    resolution:
		      type: variations
		      variants:
		        - resolution:
		            archive: tarball
		            bin: bin/node
		            integrity: sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
		            type: binary
		            url: https://nodejs.org/download/release/v0.99.1/node-v0.99.1-darwin-arm64.tar.gz
		          targets:
		            - cpu: arm64
		              os: darwin

		snapshots:

		  node@runtime:0.99.1: {}
	YAML
	run aube install
	assert_success
	run aubr which-node
	assert_success
	assert_output --partial "v0.99.1"
}

@test "lockfile pin beats a newer satisfying installed version on aubr" {
	# Regression: the warm aubr path must honor the lockfile's exact
	# pin — without pin-aware resolution it would pick the best
	# installed version in range (0.99.7) and drift from CI.
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	_fab_node "$(_aube_runtime_dir)" "0.99.7"
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" },
		  "devEngines": {
		    "runtime": { "name": "node", "version": "^0.99.0", "onFail": "download" }
		  }
		}
	JSON
	cat >aube-lock.yaml <<-'YAML'
		lockfileVersion: '9.0'

		settings:
		  autoInstallPeers: true
		  excludeLinksFromLockfile: false

		importers:

		  .:
		    devDependencies:
		      node:
		        specifier: runtime:^0.99.0
		        version: runtime:0.99.1

		packages:

		  node@runtime:0.99.1:
		    hasBin: true
		    version: 0.99.1
		    resolution:
		      type: variations
		      variants: []

		snapshots:

		  node@runtime:0.99.1: {}
	YAML
	run aube install
	assert_success
	run aubr which-node
	assert_success
	assert_output --partial "v0.99.1"
	refute_output --partial "v0.99.7"
}

@test "root preinstall hook runs on the switched node" {
	# Regression: preinstall fires before most of the install pipeline;
	# runtime resolution must already have happened by then.
	_fab_node "$(_aube_runtime_dir)" "0.99.1"
	echo "0.99" >.nvmrc
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-test",
		  "version": "0.0.0",
		  "scripts": { "preinstall": "node --version" }
		}
	JSON
	run aube install
	assert_success
	assert_output --partial "v0.99.1"
}
