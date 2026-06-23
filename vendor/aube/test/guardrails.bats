#!/usr/bin/env bats

bats_require_minimum_version 1.5.0

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_make_script_project() {
	cat >package.json <<-'JSON'
		{
		  "name": "guardrails-probe",
		  "version": "1.0.0",
		  "scripts": {
		    "ok": "echo ok",
		    "env": "node -e \"console.log('NO_COLOR=' + (process.env.NO_COLOR || '')); console.log('FORCE_COLOR=' + (process.env.FORCE_COLOR || '')); console.log('CLICOLOR_FORCE=' + (process.env.CLICOLOR_FORCE || ''))\""
		  }
		}
	JSON
}

_setup_workspace_fixture() {
	cp -r "$PROJECT_ROOT/fixtures/workspace/"* .
}

@test "color setting from .npmrc disables color for child processes" {
	_make_script_project
	echo "color=false" >.npmrc
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	run aube run env
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
	[[ "$output" != *"FORCE_COLOR=1"* ]]
}

@test "color setting from environment forces color for child processes" {
	_make_script_project
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	NPM_CONFIG_COLOR=always run aube run env
	assert_success
	[[ "$output" == *"FORCE_COLOR=1"* ]]
	[[ "$output" == *"CLICOLOR_FORCE=1"* ]]
}

@test "color setting honors --workspace-root before chdir" {
	_setup_workspace_fixture
	node <<-'NODE'
		let p = require("./package.json")
		p.scripts = { env: "node -e \"console.log('NO_COLOR=' + (process.env.NO_COLOR || ''))\"" }
		require("fs").writeFileSync("package.json", JSON.stringify(p))
	NODE
	{
		echo "color=false"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc
	cd packages/app
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	run aube --workspace-root run env
	assert_success
	[[ "$output" == *"NO_COLOR=1"* ]]
}

@test "color setting from environment works when startup cwd lookup fails" {
	_make_script_project
	unset NO_COLOR FORCE_COLOR CLICOLOR_FORCE

	NPM_CONFIG_COLOR=false run aube --workspace-root run env
	assert_failure
	[[ "$output" == *"no workspace root"* ]]
	[[ "$output" != *$'\033['* ]]
}

@test "loglevel setting from .npmrc enables debug logging" {
	_setup_basic_fixture
	echo "loglevel=debug" >.npmrc

	run --separate-stderr aube install
	assert_success
	# Match tracing's DEBUG level, not the `-DEBUG` version suffix.
	[[ "$stderr" =~ [^-]DEBUG ]]
}

@test "packageManagerStrict=warn (default) warns for run commands on unsupported package managers" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	echo "verifyDepsBeforeRun=false" >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"unsupported package manager"* ]]
	[[ "$output" == *"auto-install is disabled"* ]]
	[[ "$output" == *"ok"* ]]
	[ ! -e node_modules ]
}

@test "packageManagerStrict=warn (default) warns install on unsupported package managers without erroring" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	echo "verifyDepsBeforeRun=false" >.npmrc

	# Use `install` (not `install-test`) so exit status reflects only the
	# guard's decision: the project has no deps and no `test` script, so
	# `install` succeeds iff the warn-mode guard let it through. The
	# companion `=error` test below is the assert_failure counterpart.
	run aube install
	assert_success
	[[ "$output" == *"unsupported package manager"* ]]
}

@test "packageManagerStrict warns and falls back to default on unrecognized value" {
	_make_script_project
	{
		echo "packageManagerStrict=errror"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	# Unparseable value must emit a startup warning naming the bad
	# input rather than silently degrading to the default. The warning
	# goes to stderr (tracing isn't initialized this early), so we
	# split to keep the assertions precise.
	run --separate-stderr aube install
	assert_success
	[[ "$stderr" == *"packageManagerStrict"* ]]
	[[ "$stderr" == *"errror"* ]]
	[[ "$stderr" == *"falling back to"* ]]
}

@test "packageManagerStrict=error rejects install-test on unsupported package managers" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrict=error"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube install-test
	assert_failure
	[[ "$output" == *"unsupported package manager"* ]]
}

@test "packageManagerStrict=true (back-compat alias for error) rejects install-test" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrict=true"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube install-test
	assert_failure
	[[ "$output" == *"unsupported package manager"* ]]
}

@test "packageManagerStrict=off skips packageManager guard" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrict=off"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
	[[ "$output" != *"unsupported package manager"* ]]
}

@test "packageManagerStrict=false (back-compat alias for off) skips guard" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrict=false"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
	[[ "$output" != *"unsupported package manager"* ]]
}

@test "packageManagerStrict checks workspace root from package subdirectory" {
	_setup_workspace_fixture
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	echo "verifyDepsBeforeRun=false" >.npmrc
	cd packages/app

	run aube run start
	assert_failure
	[[ "$output" == *"unsupported package manager"* ]]
}

@test "packageManagerStrictVersion rejects mismatched aube version" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="aube@0.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrictVersion=true"
		echo "verifyDepsBeforeRun=false"
		# With managePackageManagerVersions on (the default), a
		# mismatched aube pin is *switched to*, not rejected — see
		# self_version.bats. This test pins the validation-only path.
		echo "manage-package-manager-versions=false"
	} >.npmrc

	run aube run ok
	assert_failure
	[[ "$output" == *"requires aube@0.0.0"* ]]
}

@test "packageManagerStrictVersion accepts aube version with corepack hash" {
	_make_script_project
	current="$(aube --version | awk '{print $1}')"
	node -e 'let p=require("./package.json"); p.packageManager=`aube@'"$current"'+sha512.abc123`; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrictVersion=true"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
}

@test "packageManagerStrictVersion accepts the clean version aube init writes on debug builds" {
	_make_script_project
	clean="$(aube --version | awk '{print $1}' | sed 's/-DEBUG$//')"
	node -e 'let p=require("./package.json"); p.packageManager=`aube@'"$clean"'`; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	{
		echo "packageManagerStrictVersion=true"
		echo "verifyDepsBeforeRun=false"
	} >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
}

@test "bare aube prints help without packageManager guardrail" {
	_make_script_project
	node -e 'let p=require("./package.json"); p.packageManager="yarn@4.0.0"; require("fs").writeFileSync("package.json", JSON.stringify(p))'

	run aube
	assert_success
	[[ "$output" == *"A fast Node.js package manager"* ]]
}

@test "recursiveInstall=false limits plain install to root importer" {
	_setup_workspace_fixture
	node -e 'let p=require("./package.json"); p.dependencies={"@test/lib":"workspace:*","is-number":"^6.0.0"}; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	echo "recursiveInstall=false" >.npmrc

	run aube install
	assert_success
	assert_dir_exists node_modules/@test/lib
	assert_dir_exists node_modules/is-number
	assert_not_exists packages/lib/node_modules
	assert_not_exists packages/app/node_modules
}

@test "recursiveInstall=false does not block explicit --filter" {
	_setup_workspace_fixture
	echo "recursiveInstall=false" >.npmrc

	run aube install --filter @test/lib
	assert_success
	assert_dir_exists packages/lib/node_modules/is-odd
	assert_not_exists packages/app/node_modules
}

@test "verifyDepsBeforeRun=false skips auto-install before run" {
	_make_script_project
	echo "verifyDepsBeforeRun=false" >.npmrc

	run aube run ok
	assert_success
	[[ "$output" == *"ok"* ]]
}

@test "verifyDepsBeforeRun=error fails instead of auto-installing" {
	_make_script_project
	echo "verifyDepsBeforeRun=error" >.npmrc

	run aube run ok
	assert_failure
	[[ "$output" == *"dependencies need install before run"* ]]
}

@test "verifyDepsBeforeRun=error fails after node_modules is removed" {
	_setup_basic_fixture
	node -e 'let p=require("./package.json"); p.scripts={dev:"echo hello-dev"}; require("fs").writeFileSync("package.json", JSON.stringify(p))'

	run aube install
	assert_success
	assert_dir_exists node_modules

	rm -rf node_modules
	echo "verifyDepsBeforeRun=error" >.npmrc

	run aube dev
	assert_failure
	[[ "$output" == *"dependencies need install before run"* ]]
	[[ "$output" != *"hello-dev"* ]]
}

@test "npmPath delegates npm-only fallback commands" {
	fake_npm="$TEST_TEMP_DIR/fake-npm"
	cat >"$fake_npm" <<-'SH'
		#!/usr/bin/env bash
		printf 'fake-npm %s\n' "$*"
	SH
	chmod +x "$fake_npm"
	printf 'npmPath=%s\n' "$fake_npm" >.npmrc

	run aube whoami --registry=https://registry.example.test/
	assert_success
	[[ "$output" == *"fake-npm whoami --registry=https://registry.example.test/"* ]]

	run aube stage list @scope/pkg --json --registry=https://registry.example.test/
	assert_success
	[[ "$output" == *"fake-npm stage list @scope/pkg --json --registry=https://registry.example.test/"* ]]
}

@test "npmPath fallback keeps child stderr visible under --silent" {
	fake_npm="$TEST_TEMP_DIR/fake-npm"
	cat >"$fake_npm" <<-'SH'
		#!/usr/bin/env bash
		printf 'fake-npm stderr %s\n' "$*" >&2
	SH
	chmod +x "$fake_npm"
	printf 'npmPath=%s\n' "$fake_npm" >.npmrc

	run --separate-stderr aube --silent whoami
	assert_success
	[[ "$stderr" == *"fake-npm stderr whoami"* ]]
}
