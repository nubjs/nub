#!/usr/bin/env bats
# Node runtime download paths, against a local static mirror — no
# nodejs.org traffic. Covers self-download, mise delegation, checksum
# verification, lockfile pin recording, and `aube runtime set`.

setup() {
	load 'test_helper/common_setup'
	_common_setup
	command -v python3 >/dev/null 2>&1 || skip "python3 not available for the static mirror"
	_start_mirror
}

teardown() {
	if [ -n "${MIRROR_PID:-}" ]; then
		kill "$MIRROR_PID" 2>/dev/null || true
	fi
	_common_teardown
}

_sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | awk '{print $1}'
	else
		shasum -a 256 "$1" | awk '{print $1}'
	fi
}

_host_slug() {
	local os arch
	case "$(uname -s)" in
	Darwin) os="darwin" ;;
	*) os="linux" ;;
	esac
	case "$(uname -m)" in
	arm64 | aarch64) arch="arm64" ;;
	*) arch="x64" ;;
	esac
	echo "$os-$arch"
}

# Build a fake nodejs.org dist tree for v0.95.2 (a fabricated tarball
# whose `bin/node` prints its version) and serve it over HTTP.
_start_mirror() {
	MIRROR_DIR="$TEST_TEMP_DIR/mirror"
	local version="0.95.2"
	local slug top work
	slug="$(_host_slug)"
	top="node-v$version-$slug"
	mkdir -p "$MIRROR_DIR/v$version"
	work="$TEST_TEMP_DIR/mirror-work"
	mkdir -p "$work/$top/bin"
	printf '#!/bin/sh\necho "v%s"\n' "$version" >"$work/$top/bin/node"
	chmod +x "$work/$top/bin/node"
	tar -czf "$MIRROR_DIR/v$version/$top.tar.gz" -C "$work" "$top"
	echo "$(_sha256 "$MIRROR_DIR/v$version/$top.tar.gz")  $top.tar.gz" \
		>"$MIRROR_DIR/v$version/SHASUMS256.txt"
	cat >"$MIRROR_DIR/index.json" <<-JSON
		[{"version": "v$version", "date": "2026-01-01", "files": [], "lts": false, "security": false}]
	JSON

	local log="$TEST_TEMP_DIR/mirror.log"
	python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$MIRROR_DIR" >"$log" 2>&1 &
	MIRROR_PID=$!
	# Wait for the bind line ("Serving HTTP on 127.0.0.1 port NNNN ...").
	local port=""
	for _ in $(seq 1 50); do
		port="$(sed -n 's/.*port \([0-9]*\).*/\1/p' "$log" | head -1)"
		[ -n "$port" ] && break
		sleep 0.1
	done
	[ -n "$port" ] || skip "static mirror failed to start"
	MIRROR_URL="http://127.0.0.1:$port"
	cat >aube-workspace.yaml <<-YAML
		nodeDownloadMirrors:
		  release: $MIRROR_URL
	YAML
}

_dev_engines_project() {
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-dl-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" },
		  "devEngines": {
		    "runtime": { "name": "node", "version": "^0.95.0", "onFail": "download" }
		  }
		}
	JSON
}

@test "missing version downloads from the mirror and runs scripts" {
	_dev_engines_project
	echo "runtime-installer=aube" >>.npmrc
	run aubr which-node
	assert_success
	assert_output --partial "v0.95.2"
	assert_file_exist "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
}

@test "install records the runtime pin in aube-lock.yaml" {
	_dev_engines_project
	echo "runtime-installer=aube" >>.npmrc
	run aube install
	assert_success
	assert_file_exist aube-lock.yaml
	run cat aube-lock.yaml
	assert_output --partial "specifier: runtime:^0.95.0"
	assert_output --partial "node@runtime:0.95.2"
	assert_output --partial "type: variations"
}

@test "checksum mismatch discards the download and fails" {
	_dev_engines_project
	echo "runtime-installer=aube" >>.npmrc
	# Corrupt the published checksum.
	local sums slug
	slug="$(_host_slug)"
	sums="$MIRROR_DIR/v0.95.2/SHASUMS256.txt"
	printf '%064d  node-v0.95.2-%s.tar.gz\n' 0 "$slug" >"$sums"
	run aubr which-node
	assert_failure
	assert_output --partial "checksum mismatch"
	assert_file_not_exist "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
}

@test "runtimeInstaller=auto delegates to mise when present" {
	_dev_engines_project
	# Stub mise: record argv, fabricate the install where discovery
	# looks for it.
	mkdir -p "$TEST_TEMP_DIR/stubbin"
	cat >"$TEST_TEMP_DIR/stubbin/mise" <<-STUB
		#!/bin/sh
		echo "\$@" >> "$TEST_TEMP_DIR/mise-stub.log"
		dir="$XDG_DATA_HOME/mise/installs/node/0.95.2/bin"
		mkdir -p "\$dir"
		printf '#!/bin/sh\necho "v0.95.2 (mise)"\n' > "\$dir/node"
		chmod +x "\$dir/node"
	STUB
	chmod +x "$TEST_TEMP_DIR/stubbin/mise"
	PATH="$TEST_TEMP_DIR/stubbin:$PATH" run aubr which-node
	assert_success
	assert_output --partial "v0.95.2 (mise)"
	run cat "$TEST_TEMP_DIR/mise-stub.log"
	assert_output --partial "install node@0.95.2"
	# Delegation means aube did NOT keep its own copy.
	assert_file_not_exist "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
}

@test "auto mode falls back to self-download when mise fails" {
	_dev_engines_project
	mkdir -p "$TEST_TEMP_DIR/stubbin"
	printf '#!/bin/sh\nexit 1\n' >"$TEST_TEMP_DIR/stubbin/mise"
	chmod +x "$TEST_TEMP_DIR/stubbin/mise"
	PATH="$TEST_TEMP_DIR/stubbin:$PATH" run aubr which-node
	assert_success
	assert_output --partial "v0.95.2"
	assert_file_exist "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
}

@test "runtimeInstaller=mise fails hard when mise fails" {
	_dev_engines_project
	echo "runtime-installer=mise" >>.npmrc
	mkdir -p "$TEST_TEMP_DIR/stubbin"
	printf '#!/bin/sh\nexit 7\n' >"$TEST_TEMP_DIR/stubbin/mise"
	chmod +x "$TEST_TEMP_DIR/stubbin/mise"
	PATH="$TEST_TEMP_DIR/stubbin:$PATH" run aubr which-node
	assert_failure
	assert_output --partial "mise failed to install node@0.95.2"
	assert_file_not_exist "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
}

@test "aube runtime set pins manifest and lockfile" {
	echo '{"name": "rt-set", "version": "0.0.0"}' >package.json
	echo "runtime-installer=aube" >>.npmrc
	run aube runtime set node 0.95
	assert_success
	run cat package.json
	assert_output --partial '"name": "node"'
	assert_output --partial '"version": "^0.95.2"'
	assert_output --partial '"onFail": "download"'
	run cat aube-lock.yaml
	assert_output --partial "node@runtime:0.95.2"
	assert_file_exist "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
}

@test "lockfile pin round-trips: second install reuses the pinned version offline" {
	_dev_engines_project
	echo "runtime-installer=aube" >>.npmrc
	run aube install
	assert_success
	# Kill the mirror: the pinned version is installed, so the warm
	# path must resolve with zero network.
	kill "$MIRROR_PID" 2>/dev/null || true
	wait "$MIRROR_PID" 2>/dev/null || true
	MIRROR_PID=""
	run aubr which-node
	assert_success
	assert_output --partial "v0.95.2"
}

@test "alias spec with onFail=warn consults the index before warning" {
	# Regression: `latest`/`lts` satisfaction is index-dependent. With
	# onFail=warn, aube must resolve the alias (the mirror says latest
	# is 0.95.2), notice the installed 0.95.2 satisfies it, and switch
	# — not emit a false mismatch warning and stay on PATH node.
	mkdir -p "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin"
	printf '#!/bin/sh\necho "v0.95.2"\n' >"$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
	chmod +x "$XDG_DATA_HOME/aube/nodejs/0.95.2/bin/node"
	cat >package.json <<-'JSON'
		{
		  "name": "runtime-dl-test",
		  "version": "0.0.0",
		  "scripts": { "which-node": "node --version" },
		  "devEngines": {
		    "runtime": { "name": "node", "version": "latest", "onFail": "warn" }
		  }
		}
	JSON
	run aubr which-node
	assert_success
	assert_output --partial "v0.95.2"
	refute_output --partial "does not satisfy"
}
