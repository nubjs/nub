#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube run executes a script" {
	_setup_basic_fixture
	aube install
	run aube run hello
	assert_success
	assert_output --partial "hello from aube!"
}

@test "aube run test executes node script" {
	_setup_basic_fixture
	aube install
	run aube run test
	assert_success
	assert_output --partial "is-odd(3): true"
}

@test "aube run forwards inspect flags to direct node scripts" {
	cat >package.json <<-'JSON'
		{
		  "name": "run-inspect-script",
		  "version": "1.0.0",
		  "private": true,
		  "scripts": {
		    "show-argv": "node show-argv.js"
		  }
		}
	JSON
	cat >show-argv.js <<-'JS'
		console.log(JSON.stringify(process.execArgv))
	JS

	run aube run --inspect=0 --no-install show-argv
	assert_success
	assert_output --partial '"--inspect=0"'
}

@test "aube run fails for unknown script" {
	_setup_basic_fixture
	aube install
	run aube run nonexistent
	assert_failure
	assert_output --partial "script not found"
}

@test "aube run falls back to local binary when no script matches" {
	mkdir -p tools/local-bin
	cat >package.json <<-'JSON'
		{
		  "name": "run-bin-fallback",
		  "version": "1.0.0",
		  "private": true,
		  "dependencies": {
		    "local-bin": "file:tools/local-bin"
		  }
		}
	JSON
	cat >tools/local-bin/package.json <<-'JSON'
		{
		  "name": "local-bin",
		  "version": "1.0.0",
		  "bin": {
		    "local-bin": "index.js"
		  }
		}
	JSON
	cat >tools/local-bin/index.js <<-'JS'
		#!/usr/bin/env node
		console.log(`local-bin:${process.argv.slice(2).join(",")}`)
	JS
	chmod +x tools/local-bin/index.js

	aube install
	run aube run local-bin alpha beta
	assert_success
	assert_line "local-bin:alpha,beta"
}

@test "aube run forwards inspect flags to local node binaries" {
	mkdir -p tools/local-bin
	cat >package.json <<-'JSON'
		{
		  "name": "run-bin-inspect",
		  "version": "1.0.0",
		  "private": true,
		  "dependencies": {
		    "local-bin": "file:tools/local-bin"
		  }
		}
	JSON
	cat >tools/local-bin/package.json <<-'JSON'
		{
		  "name": "local-bin",
		  "version": "1.0.0",
		  "bin": {
		    "local-bin": "index.js"
		  }
		}
	JSON
	cat >tools/local-bin/index.js <<-'JS'
		#!/usr/bin/env node
		console.log(JSON.stringify(process.execArgv))
	JS
	chmod +x tools/local-bin/index.js

	aube install
	run aube run --inspect=0 local-bin
	assert_success
	assert_output --partial '"--inspect=0"'
}

@test "aube run --if-present still falls back to local binary" {
	mkdir -p tools/local-bin
	cat >package.json <<-'JSON'
		{
		  "name": "run-bin-if-present",
		  "version": "1.0.0",
		  "private": true,
		  "dependencies": {
		    "local-bin": "file:tools/local-bin"
		  }
		}
	JSON
	cat >tools/local-bin/package.json <<-'JSON'
		{
		  "name": "local-bin",
		  "version": "1.0.0",
		  "bin": {
		    "local-bin": "index.js"
		  }
		}
	JSON
	cat >tools/local-bin/index.js <<-'JS'
		#!/usr/bin/env node
		console.log(`local-bin-if-present:${process.argv.slice(2).join(",")}`)
	JS
	chmod +x tools/local-bin/index.js

	aube install
	run aube run --if-present local-bin alpha beta
	assert_success
	assert_line "local-bin-if-present:alpha,beta"
}

@test "aube run filtered falls back to local binaries" {
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	cat >package.json <<-'JSON'
		{"name":"root","version":"0.0.0","private":true}
	JSON
	mkdir -p packages/a packages/b tools/local-bin
	cat >packages/a/package.json <<-'JSON'
		{"name":"a","version":"0.0.0","dependencies":{"local-bin":"file:../../tools/local-bin"}}
	JSON
	cat >packages/b/package.json <<-'JSON'
		{"name":"b","version":"0.0.0","dependencies":{"local-bin":"file:../../tools/local-bin"}}
	JSON
	cat >tools/local-bin/package.json <<-'JSON'
		{
		  "name": "local-bin",
		  "version": "1.0.0",
		  "bin": {
		    "local-bin": "index.js"
		  }
		}
	JSON
	cat >tools/local-bin/index.js <<-'JS'
		#!/usr/bin/env node
		console.log(`filtered-bin:${process.cwd().split("/").pop()}`)
	JS
	chmod +x tools/local-bin/index.js

	aube install
	run aube -r run local-bin
	assert_success
	assert_output --partial "filtered-bin:a"
	assert_output --partial "filtered-bin:b"
}

@test "aube run filtered parallel falls back to local binaries" {
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	cat >package.json <<-'JSON'
		{"name":"root","version":"0.0.0","private":true}
	JSON
	mkdir -p packages/a packages/b tools/local-bin
	cat >packages/a/package.json <<-'JSON'
		{"name":"a","version":"0.0.0","dependencies":{"local-bin":"file:../../tools/local-bin"}}
	JSON
	cat >packages/b/package.json <<-'JSON'
		{"name":"b","version":"0.0.0","dependencies":{"local-bin":"file:../../tools/local-bin"}}
	JSON
	cat >tools/local-bin/package.json <<-'JSON'
		{
		  "name": "local-bin",
		  "version": "1.0.0",
		  "bin": {
		    "local-bin": "index.js"
		  }
		}
	JSON
	cat >tools/local-bin/index.js <<-'JS'
		#!/usr/bin/env node
		console.log(`filtered-parallel-bin:${process.cwd().split("/").pop()}`)
	JS
	chmod +x tools/local-bin/index.js

	aube install
	run aube -r run --parallel local-bin
	assert_success
	assert_output --partial "filtered-parallel-bin:a"
	assert_output --partial "filtered-parallel-bin:b"
}

@test "aube run without a script errors with available scripts when stdin isn't a TTY" {
	_setup_basic_fixture
	aube install
	run aube run </dev/null
	assert_failure
	assert_output --partial "script name required"
	# Fixture defines scripts in `test, hello` order; assert the error
	# preserves definition order (not alphabetical, which would put
	# `hello` first).
	assert_output --regexp 'Available scripts:.*test.*hello'
}

@test "aube run --if-present exits 0 for unknown script" {
	_setup_basic_fixture
	aube install
	run aube run --if-present nonexistent
	assert_success
	refute_output --partial "script not found"
}

@test "aube run --if-present still runs the script when present" {
	_setup_basic_fixture
	aube install
	run aube run --if-present hello
	assert_success
	assert_output --partial "hello from aube!"
}

@test "aube run auto-installs when node_modules missing" {
	_setup_basic_fixture
	# Don't install first — aube run should auto-install
	run aube run hello
	assert_success
	assert_output --partial "Auto-installing"
	assert_output --partial "hello from aube!"
}

@test "aube run skips install when deps are current" {
	_setup_basic_fixture
	aube install
	# Second run should NOT auto-install
	run aube run hello
	assert_success
	refute_output --partial "Auto-installing"
	assert_output --partial "hello from aube!"
}

@test "aube run from workspace subpackage reuses root install state" {
	# Regression: ensure_installed used to anchor its freshness check
	# at the nearest package.json (the subpackage) and miss the state
	# file that install writes only at the workspace root. Result: every
	# `aube run` / `aube start` from a subpackage spuriously reported
	# "install state not found" and re-ran install.
	cp -r "$PROJECT_ROOT/fixtures/workspace/"* .
	aube install
	cd packages/app
	run aube start
	assert_success
	refute_output --partial "Auto-installing"
}

@test "aube run auto-installs when package.json changes" {
	_setup_basic_fixture
	aube install
	# Modify package.json to trigger staleness
	echo '{"name":"modified","version":"1.0.0","scripts":{"hello":"echo modified"},"dependencies":{"is-odd":"^3.0.1","is-even":"^1.0.0"}}' >package.json
	run aube run hello
	assert_success
	assert_output --partial "Auto-installing"
	assert_output --partial "modified"
}

@test "aube run --no-install skips auto-install" {
	_setup_basic_fixture
	# Don't install, use --no-install with a script that needs node_modules
	run aube run --no-install test
	# Script should fail since node_modules doesn't exist (require fails)
	assert_failure
}

@test "AUBE_NO_AUTO_INSTALL env var skips auto-install" {
	_setup_basic_fixture
	AUBE_NO_AUTO_INSTALL=1 run aube run test
	# Should fail since node_modules doesn't exist
	assert_failure
}

@test ".npmrc aubeNoAutoInstall=true skips auto-install" {
	# Exercises the new `.npmrc` source for the `aubeNoAutoInstall`
	# setting. If the typed accessor weren't plumbed through `.npmrc`,
	# auto-install would kick in and the `require("is-odd")` in the
	# basic fixture's `test` script would succeed, contradicting the
	# assertion below.
	_setup_basic_fixture
	echo "aubeNoAutoInstall=true" >.npmrc
	run aube run test
	# Should fail since node_modules doesn't exist — auto-install was skipped.
	assert_failure
}

@test "aube-workspace.yaml aubeNoAutoInstall skips auto-install" {
	# Exercises the workspace-yaml source.
	_setup_basic_fixture
	cat >aube-workspace.yaml <<-EOF
		packages: []
		aubeNoAutoInstall: true
	EOF
	run aube run test
	assert_failure
}

@test "aube run chains pre and post scripts by default" {
	cat >package.json <<'JSON'
{
  "name": "run-pre-post-test",
  "version": "1.0.0",
  "scripts": {
    "prebuild": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"pre\\n\")'",
    "build": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"build\\n\")'",
    "postbuild": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"post\\n\")'"
  }
}
JSON
	run aube run build
	assert_success
	run cat order.log
	assert_output "pre
build
post"
}

@test "enablePrePostScripts=false disables run pre and post chaining" {
	cat >.npmrc <<'EOF'
enablePrePostScripts=false
EOF
	cat >package.json <<'JSON'
{
  "name": "run-pre-post-disabled-test",
  "version": "1.0.0",
  "scripts": {
    "prebuild": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"pre\\n\")'",
    "build": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"build\\n\")'",
    "postbuild": "node -e 'require(\"fs\").appendFileSync(\"order.log\", \"post\\n\")'"
  }
}
JSON
	run aube run build
	assert_success
	run cat order.log
	assert_output "build"
}

@test "aube run applies script environment settings" {
	cat >shell-wrapper.sh <<'EOF'
#!/bin/sh
echo custom-shell >> shell.log
exec /bin/sh "$@"
EOF
	chmod +x shell-wrapper.sh
	cat >.npmrc <<EOF
nodeOptions=--no-warnings
scriptShell=$PWD/shell-wrapper.sh
shellEmulator=true
unsafePerm=true
EOF
	cat >package.json <<'JSON'
{
  "name": "run-script-settings-test",
  "version": "1.0.0",
  "scripts": {
    "env": "node -e 'console.log(process.env.NODE_OPTIONS); console.log(process.env.npm_config_unsafe_perm); console.log(process.env.npm_config_shell_emulator)'"
  }
}
JSON
	run aube run env
	assert_success
	assert_output --partial "--no-warnings"
	assert_output --partial "true"
	assert_file_exists shell.log
}

@test "aube run exports the full npm_* env set (pnpm parity)" {
	# A pnpm/aube bridge keys off these npm_* vars to detect the running
	# PM (npm_execpath) and read package metadata. aube used to omit
	# npm_execpath, npm_node_execpath, npm_package_json, npm_command,
	# npm_config_node_gyp, npm_package_engines_*, and npm_lifecycle_script
	# on the `run` path; assert they're all present and well-formed now.
	cat >package.json <<'JSON'
{
  "name": "@scope/run-env-probe",
  "version": "2.0.1",
  "engines": { "node": ">=18.0.0" },
  "scripts": {
    "probe": "node -e 'for (const k of [\"npm_execpath\",\"npm_node_execpath\",\"npm_package_json\",\"npm_command\",\"npm_config_node_gyp\",\"npm_package_engines_node\",\"npm_package_name\",\"npm_package_version\",\"npm_lifecycle_script\"]) console.log(k + \"=\" + (process.env[k] || \"\"))'"
  }
}
JSON
	run aube run probe
	assert_success
	# npm_command is "run-script" for every script-running command.
	assert_output --partial "npm_command=run-script"
	# npm_execpath points back at the aube binary that drove the script.
	assert_output --regexp "npm_execpath=[^[:space:]]*aube"
	# npm_node_execpath / NODE resolve to a node binary (non-empty).
	assert_output --regexp "npm_node_execpath=[^[:space:]]+"
	# Absolute path to the package.json being run.
	assert_output --regexp "npm_package_json=[^[:space:]]*package\.json"
	# Lazy node-gyp stand-in for npm_config_node_gyp parity.
	assert_output --regexp "npm_config_node_gyp=[^[:space:]]*node-gyp\.js"
	# Deep-flattened manifest fields.
	assert_output --partial "npm_package_engines_node=>=18.0.0"
	assert_output --partial "npm_package_name=@scope/run-env-probe"
	assert_output --partial "npm_package_version=2.0.1"
	# Raw script body (pnpm exports this for tools that re-run it).
	assert_output --regexp "npm_lifecycle_script=.*node -e"
}

# discussion #228: a package's own `bin` should resolve from its own
# scripts without `npx`, matching yarn/pnpm behavior.
@test "aube run resolves package's own bin (string form)" {
	cat >bin.js <<'EOF'
#!/usr/bin/env node
console.log("self-bin:", process.argv.slice(2).join(" "))
EOF
	chmod +x bin.js
	cat >package.json <<'JSON'
{
  "name": "my-cli-app",
  "version": "1.0.0",
  "bin": "./bin.js",
  "scripts": { "self": "my-cli-app hello" }
}
JSON
	aube install
	assert_file_exists node_modules/.bin/my-cli-app
	run aube run self
	assert_success
	assert_output --partial "self-bin: hello"
}

@test "aube run resolves package's own bin (object form)" {
	cat >foo.js <<'EOF'
#!/usr/bin/env node
console.log("foo!")
EOF
	cat >bar.js <<'EOF'
#!/usr/bin/env node
console.log("bar!")
EOF
	chmod +x foo.js bar.js
	cat >package.json <<'JSON'
{
  "name": "multi-bin",
  "version": "1.0.0",
  "bin": { "foo": "./foo.js", "bar": "./bar.js" },
  "scripts": { "run-both": "foo && bar" }
}
JSON
	aube install
	assert_file_exists node_modules/.bin/foo
	assert_file_exists node_modules/.bin/bar
	run aube run run-both
	assert_success
	assert_output --partial "foo!"
	assert_output --partial "bar!"
}

# discussion #228 follow-up: the bin target is often a build output
# restored from `actions/upload-artifact` / `download-artifact`, which
# strips the POSIX exec bit. A symlink-based self-bin would then hit
# `Permission denied` at exec time. Writing a POSIX shim makes the
# target's exec bit irrelevant.
@test "aube run self-bin works when target lacks exec bit" {
	mkdir -p dist
	cat >dist/bin.js <<'EOF'
#!/usr/bin/env node
console.log("built-bin")
EOF
	chmod -x dist/bin.js
	cat >package.json <<'JSON'
{
  "name": "built-cli",
  "version": "1.0.0",
  "bin": "./dist/bin.js",
  "scripts": { "self": "built-cli" }
}
JSON
	aube install
	run aube run self
	assert_success
	assert_output --partial "built-bin"
}

# Matches the tstyche CI flow: `aube ci` runs before `dist/` is
# materialized (later downloaded from an artifact), so the self-bin
# target does not exist at install time.
@test "aube run self-bin works when target is absent at install time" {
	cat >package.json <<'JSON'
{
  "name": "artifact-cli",
  "version": "1.0.0",
  "bin": "./dist/bin.js",
  "scripts": { "self": "artifact-cli" }
}
JSON
	aube install
	mkdir -p dist
	cat >dist/bin.js <<'EOF'
#!/usr/bin/env node
console.log("late-bin")
EOF
	chmod -x dist/bin.js
	run aube run self
	assert_success
	assert_output --partial "late-bin"
}

@test "aube run resolves workspace member's own bin" {
	cat >package.json <<'JSON'
{ "name": "root", "version": "1.0.0" }
JSON
	cat >pnpm-workspace.yaml <<'YAML'
packages:
  - "packages/*"
YAML
	mkdir -p packages/cli
	cat >packages/cli/bin.js <<'EOF'
#!/usr/bin/env node
console.log("cli-bin")
EOF
	chmod +x packages/cli/bin.js
	cat >packages/cli/package.json <<'JSON'
{
  "name": "my-cli",
  "version": "1.0.0",
  "bin": "./bin.js",
  "scripts": { "self": "my-cli" }
}
JSON
	aube install
	assert_file_exists packages/cli/node_modules/.bin/my-cli
	run aube -C packages/cli run self
	assert_success
	assert_output --partial "cli-bin"
}
