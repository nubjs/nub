#!/usr/bin/env bats
# aube self-version management (managePackageManagerVersions):
# `packageManager` / `devEngines.packageManager` pins re-exec the
# pinned aube. Hermetic — pinned "versions" are fabricated shell
# scripts in the mise installs dir, so no downloads happen.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_mise_aube_dir() {
	echo "$XDG_DATA_HOME/mise/installs/aube"
}

# Fabricate an installed "aube" (and the aubr/aubx multicall siblings)
# that announces which binary and version answered.
_fab_aube() {
	local root="$1" version="$2"
	local dir="$root/$version"
	mkdir -p "$dir"
	for bin in aube aubr aubx; do
		printf '#!/bin/sh\necho "FAKE-%s %s args: $*"\n' \
			"$(echo "$bin" | tr '[:lower:]' '[:upper:]')" "$version" >"$dir/$bin"
		chmod +x "$dir/$bin"
	done
}

_running_version() {
	aube --version | awk '{print $1}' | sed 's/-DEBUG$//'
}

@test "packageManager pin re-execs the pinned aube" {
	_fab_aube "$(_mise_aube_dir)" "9.9.9"
	cat >package.json <<-'JSON'
		{ "name": "t", "version": "0.0.0", "packageManager": "aube@9.9.9" }
	JSON
	run aube install
	assert_success
	assert_output --partial "FAKE-AUBE 9.9.9 args: install"
}

@test "multicall name survives the re-exec" {
	_fab_aube "$(_mise_aube_dir)" "9.9.9"
	cat >package.json <<-'JSON'
		{ "name": "t", "version": "0.0.0", "packageManager": "aube@9.9.9", "scripts": {"x": "true"} }
	JSON
	run aubr x
	assert_success
	assert_output --partial "FAKE-AUBR 9.9.9 args: x"
}

@test "satisfied pin does not re-exec" {
	_fab_aube "$(_mise_aube_dir)" "$(_running_version)"
	cat >package.json <<-JSON
		{ "name": "t", "version": "0.0.0", "packageManager": "aube@$(_running_version)" }
	JSON
	run aube install
	assert_success
	refute_output --partial "FAKE-AUBE"
}

@test "devEngines.packageManager range picks the best installed version" {
	_fab_aube "$(_mise_aube_dir)" "9.9.1"
	_fab_aube "$(_mise_aube_dir)" "9.9.5"
	cat >package.json <<-'JSON'
		{
		  "name": "t",
		  "version": "0.0.0",
		  "devEngines": { "packageManager": { "name": "aube", "version": "^9.9.0" } }
		}
	JSON
	run aube install
	assert_success
	assert_output --partial "FAKE-AUBE 9.9.5"
}

@test "managePackageManagerVersions=false disables switching" {
	_fab_aube "$(_mise_aube_dir)" "9.9.9"
	echo "manage-package-manager-versions=false" >>.npmrc
	cat >package.json <<-'JSON'
		{ "name": "t", "version": "0.0.0", "packageManager": "aube@9.9.9" }
	JSON
	run aube install
	assert_success
	refute_output --partial "FAKE-AUBE"
}

@test "loop guard prevents repeated re-exec" {
	_fab_aube "$(_mise_aube_dir)" "9.9.9"
	cat >package.json <<-'JSON'
		{ "name": "t", "version": "0.0.0", "packageManager": "aube@9.9.9" }
	JSON
	# Simulate "already switched once": the guard env is set but the
	# running binary still mismatches. Must run here, not re-exec.
	AUBE_SELF_SWITCHED=9.9.9 run aube install
	assert_success
	refute_output --partial "FAKE-AUBE"
}

@test "non-aube packageManager pins are not switched" {
	_fab_aube "$(_mise_aube_dir)" "9.9.9"
	cat >package.json <<-'JSON'
		{ "name": "t", "version": "0.0.0", "packageManager": "pnpm@10.4.1" }
	JSON
	run aube install
	assert_success
	refute_output --partial "FAKE-AUBE"
}

@test "onFail=error fails when the pinned version is not installed" {
	cat >package.json <<-'JSON'
		{
		  "name": "t",
		  "version": "0.0.0",
		  "devEngines": { "packageManager": { "name": "aube", "version": "9.8.7", "onFail": "error" } }
		}
	JSON
	run aube install
	assert_failure
	assert_output --partial "aube@9.8.7 is not installed"
}

@test "onFail=warn keeps the running aube" {
	cat >package.json <<-'JSON'
		{
		  "name": "t",
		  "version": "0.0.0",
		  "devEngines": { "packageManager": { "name": "aube", "version": "9.8.7", "onFail": "warn" } }
		}
	JSON
	run aube install
	assert_success
	refute_output --partial "FAKE-AUBE"
}

@test "incomplete and symlinked mise installs are not switch targets" {
	_fab_aube "$(_mise_aube_dir)" "9.9.9"
	touch "$(_mise_aube_dir)/9.9.9/incomplete"
	ln -s "$(_mise_aube_dir)/9.9.9" "$(_mise_aube_dir)/latest"
	cat >package.json <<-'JSON'
		{
		  "name": "t",
		  "version": "0.0.0",
		  "devEngines": { "packageManager": { "name": "aube", "version": "9.9.9", "onFail": "warn" } }
		}
	JSON
	run aube install
	assert_success
	refute_output --partial "FAKE-AUBE"
}

@test "doctor reports the aube pin" {
	cat >package.json <<-JSON
		{ "name": "t", "version": "0.0.0", "packageManager": "aube@$(_running_version)" }
	JSON
	run aube doctor
	assert_success
	assert_output --partial "aube-pin"
	assert_output --partial "via packageManager"
}
