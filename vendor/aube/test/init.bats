#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube init writes a default package.json" {
	mkdir demo-proj
	cd demo-proj
	run aube init
	assert_success
	assert [ -f package.json ]
	run node -e 'const p = require("./package.json"); console.log(p.name, p.version, p.main, p.license, p.scripts.test)'
	assert_success
	assert_output "demo-proj 1.0.0 index.js ISC echo \"Error: no test specified\" && exit 1"
}

@test "aube init --bare writes only name and version" {
	mkdir bare-proj
	cd bare-proj
	run aube init --bare
	assert_success
	run node -e 'const p = require("./package.json"); console.log(Object.keys(p).join(","))'
	assert_success
	assert_output "name,version"
}

@test "aube init --init-type module sets type" {
	mkdir mod-proj
	cd mod-proj
	run aube init --init-type module
	assert_success
	run node -e 'console.log(require("./package.json").type)'
	assert_output "module"
}

@test "aube init --init-package-manager pins aube" {
	mkdir pm-proj
	cd pm-proj
	run aube init --init-package-manager
	assert_success
	run node -e 'console.log(require("./package.json").packageManager)'
	assert_output --partial "aube@"
}

@test "aube init fails when package.json already exists" {
	echo '{"name":"existing","version":"0.0.1"}' >package.json
	run aube init
	assert_failure
	assert_output --partial "already exists"
}
