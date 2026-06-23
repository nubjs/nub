#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_setup_filter_workspace() {
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - packages/*
	EOF
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	mkdir -p packages/lib-a packages/lib-b packages/other
	cat >packages/lib-a/package.json <<-'EOF'
		{
		  "name": "@scope/lib-a",
		  "version": "1.0.0",
		  "dependencies": { "@scope/lib-b": "workspace:*" },
		  "scripts": { "hello": "echo lib-a-ran" }
		}
	EOF
	cat >packages/lib-b/package.json <<-'EOF'
		{
		  "name": "@scope/lib-b",
		  "version": "1.0.0",
		  "scripts": { "hello": "echo lib-b-ran" }
		}
	EOF
	cat >packages/other/package.json <<-'EOF'
		{
		  "name": "other",
		  "version": "1.0.0",
		  "scripts": { "hello": "echo other-ran" }
		}
	EOF
}

@test "aube -F run: exact-name selector runs script in one package" {
	_setup_filter_workspace
	run aube -F @scope/lib-a run hello
	assert_success
	assert_output --partial "lib-a-ran"
	refute_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -F run: glob selector fans out to multiple packages" {
	_setup_filter_workspace
	run aube -F '@scope/*' run hello
	assert_success
	assert_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -F run: dependency graph selector includes workspace deps" {
	_setup_filter_workspace
	run aube -F '@scope/lib-a...' run hello
	assert_success
	assert_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -F run: dependency graph selector can exclude the seed" {
	_setup_filter_workspace
	run aube -F '@scope/lib-a^...' run hello
	assert_success
	refute_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -F run: dependent graph selector includes workspace dependents" {
	_setup_filter_workspace
	run aube -F '...@scope/lib-b' run hello
	assert_success
	assert_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -F run: exclusion selector subtracts from recursive match" {
	_setup_filter_workspace
	run aube -F '*' -F '!@scope/lib-b' run hello
	assert_success
	assert_output --partial "lib-a-ran"
	refute_output --partial "lib-b-ran"
	assert_output --partial "other-ran"
}

@test "aube -F run: exclusion selector is order independent" {
	_setup_filter_workspace
	run aube -F '!@scope/lib-b' -F '*' run hello
	assert_success
	assert_output --partial "lib-a-ran"
	refute_output --partial "lib-b-ran"
	assert_output --partial "other-ran"
}

@test "aube -F run: git-ref selector matches committed changed packages" {
	_setup_filter_workspace
	git init
	git -c user.email=test@example.com -c user.name=Test add -A
	git -c user.email=test@example.com -c user.name=Test commit -m init

	echo "// committed change" >>packages/lib-a/index.js
	git -c user.email=test@example.com -c user.name=Test add packages/lib-a/index.js
	git -c user.email=test@example.com -c user.name=Test commit -m "change lib-a"
	echo "// uncommitted change" >>packages/lib-b/index.js

	run aube -F '[HEAD~1]' run hello
	assert_success
	assert_output --partial "lib-a-ran"
	refute_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -F run: git-ref selector works in nested git repo workspace" {
	git init
	mkdir frontend
	(
		cd frontend
		_setup_filter_workspace
	)
	git -c user.email=test@example.com -c user.name=Test add -A
	git -c user.email=test@example.com -c user.name=Test commit -m init

	echo "// committed change" >>frontend/packages/lib-a/index.js
	git -c user.email=test@example.com -c user.name=Test add frontend/packages/lib-a/index.js
	git -c user.email=test@example.com -c user.name=Test commit -m "change lib-a"

	(
		cd frontend
		run aube -F '[HEAD~1]' run hello
		assert_success
		assert_output --partial "lib-a-ran"
		refute_output --partial "lib-b-ran"
		refute_output --partial "other-ran"
	)
}

@test "aube -F run: path selector matches directory prefix" {
	_setup_filter_workspace
	run aube --filter ./packages/lib-b run hello
	assert_success
	assert_output --partial "lib-b-ran"
	refute_output --partial "lib-a-ran"
}

@test "aube install --filter keeps root devDependencies" {
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - packages/*
	EOF
	cat >package.json <<-'EOF'
		{
		  "name": "root",
		  "version": "1.0.0",
		  "private": true,
		  "devDependencies": {
		    "is-odd": "^3.0.1"
		  }
		}
	EOF
	mkdir -p packages/app
	cat >packages/app/package.json <<-'EOF'
		{
		  "name": "@scope/app",
		  "version": "1.0.0",
		  "dependencies": {
		    "is-even": "^1.0.0"
		  }
		}
	EOF

	run aube install --filter '@scope/app...'
	assert_success
	assert_link_exists node_modules/is-odd
	run node -e 'console.log(require.resolve("is-odd"))'
	assert_success
	assert_output --partial "node_modules/is-odd"
}

@test "aube install --filter <member>... scopes to the member and its workspace deps (sharedWorkspaceLockfile=false)" {
	# Mirrors a per-project-lockfile monorepo. A plain `aube install` from
	# anywhere installs every importer (recursiveInstall defaults to true,
	# matching pnpm). To install just one service plus the workspace
	# siblings it depends on — without touching unrelated members — scope
	# with `--filter <member>...`; the trailing `...` pulls in deps so the
	# symlinked sibling's own node_modules get populated too.
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - packages/*
		sharedWorkspaceLockfile: false
	EOF
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	mkdir -p packages/svc-a packages/lib packages/svc-b
	cat >packages/svc-a/package.json <<-'EOF'
		{
		  "name": "svc-a",
		  "version": "1.0.0",
		  "dependencies": { "lib": "workspace:*", "is-odd": "3.0.1" }
		}
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "lib",
		  "version": "1.0.0",
		  "dependencies": { "is-number": "6.0.0" }
		}
	EOF
	cat >packages/svc-b/package.json <<-'EOF'
		{
		  "name": "svc-b",
		  "version": "1.0.0",
		  "dependencies": { "is-even": "1.0.0" }
		}
	EOF

	run aube install --filter 'svc-a...'
	assert_success

	# svc-a's own deps are linked.
	assert_link_exists packages/svc-a/node_modules/is-odd
	# The workspace sibling is symlinked *and* its own deps are populated,
	# so svc-a -> lib -> is-number resolves at runtime.
	assert_link_exists packages/svc-a/node_modules/lib
	assert_link_exists packages/lib/node_modules/is-number

	# The unrelated member is left untouched: no deps linked, no lockfile.
	run test -e packages/svc-b/node_modules/is-even
	assert_failure
	run test -e packages/svc-b/aube-lock.yaml
	assert_failure
}

@test "aube install --filter <member> (no dots) skips unrelated members (sharedWorkspaceLockfile=false)" {
	# pnpm parity: without the trailing `...` only the named member is
	# installed. The sibling is still symlinked (it's a direct dep of the
	# selected member) but its transitive deps are not pulled in, and
	# unrelated members are never touched.
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - packages/*
		sharedWorkspaceLockfile: false
	EOF
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	mkdir -p packages/svc-a packages/svc-b
	cat >packages/svc-a/package.json <<-'EOF'
		{"name": "svc-a", "version": "1.0.0", "dependencies": { "is-odd": "3.0.1" }}
	EOF
	cat >packages/svc-b/package.json <<-'EOF'
		{"name": "svc-b", "version": "1.0.0", "dependencies": { "is-even": "1.0.0" }}
	EOF

	run aube install --filter 'svc-a'
	assert_success
	assert_link_exists packages/svc-a/node_modules/is-odd
	run test -e packages/svc-b/node_modules/is-even
	assert_failure
}

@test "aube -F run: unmatched selector errors" {
	_setup_filter_workspace
	run aube -F no-such-pkg run hello
	assert_failure
	assert_output --partial "did not match"
}

@test "aube -F run: shortcut commands honor filter" {
	_setup_filter_workspace
	# `hello` is not a shortcut; use the implicit-script path via `aube hello`,
	# which routes through run_script and should pick up --filter too.
	run aube -F @scope/lib-a hello
	assert_success
	assert_output --partial "lib-a-ran"
	refute_output --partial "lib-b-ran"
}

@test "aube -r run: fans out to every workspace package" {
	_setup_filter_workspace
	run aube -r run hello
	assert_success
	assert_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	assert_output --partial "other-ran"
}

@test "aube --recursive run: long form matches short" {
	_setup_filter_workspace
	run aube --recursive run hello
	assert_success
	assert_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	assert_output --partial "other-ran"
}

@test "aube recursive run: wrapper form fans out to every workspace package" {
	_setup_filter_workspace
	run aube recursive run hello
	assert_success
	assert_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	assert_output --partial "other-ran"
}

@test "aube recursive run: wrapper preserves explicit filter" {
	_setup_filter_workspace
	run aube -F @scope/lib-a recursive run hello
	assert_success
	assert_output --partial "lib-a-ran"
	refute_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -r: explicit filter wins over recursive" {
	_setup_filter_workspace
	run aube -r -F @scope/lib-a run hello
	assert_success
	assert_output --partial "lib-a-ran"
	refute_output --partial "lib-b-ran"
	refute_output --partial "other-ran"
}

@test "aube -r: implicit script fanout" {
	_setup_filter_workspace
	run aube -r hello
	assert_success
	assert_output --partial "lib-a-ran"
	assert_output --partial "lib-b-ran"
	assert_output --partial "other-ran"
}

@test "aube -F update: --global uses global installs instead of workspace filter" {
	_setup_filter_workspace
	cat >packages/lib-a/package.json <<-'EOF'
		{
		  "name": "@scope/lib-a",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^0.1.0" },
		  "scripts": { "hello": "echo lib-a-ran" }
		}
	EOF

	run aube -F @scope/lib-a update --global is-odd
	assert_failure
	assert_output --partial "no global packages installed"

	run grep -F '"is-odd": "^0.1.0"' packages/lib-a/package.json
	assert_success
}

# --filter-prod tests: the workspace below gives api a prod edge to lib
# and a dev-only edge to tooling. `--filter-prod 'api...'` must skip the
# dev edge, so the graph walk reaches lib but not tooling. Exact-name
# and glob selectors ignore edges entirely, so they match pnpm's
# `--filter-prod` behavior of treating the non-graph forms the same as
# `--filter`.
_setup_filter_prod_workspace() {
	cat >pnpm-workspace.yaml <<-EOF
		packages:
		  - packages/*
	EOF
	cat >package.json <<-'EOF'
		{"name": "root", "version": "0.0.0", "private": true}
	EOF
	mkdir -p packages/api packages/lib packages/tooling
	cat >packages/api/package.json <<-'EOF'
		{
		  "name": "@scope/api",
		  "version": "1.0.0",
		  "dependencies": { "@scope/lib": "workspace:*" },
		  "devDependencies": { "@scope/tooling": "workspace:*" },
		  "scripts": { "hello": "echo api-ran" }
		}
	EOF
	cat >packages/lib/package.json <<-'EOF'
		{
		  "name": "@scope/lib",
		  "version": "1.0.0",
		  "scripts": { "hello": "echo lib-ran" }
		}
	EOF
	cat >packages/tooling/package.json <<-'EOF'
		{
		  "name": "@scope/tooling",
		  "version": "1.0.0",
		  "scripts": { "hello": "echo tooling-ran" }
		}
	EOF
}

@test "aube --filter-prod: graph selector skips devDependencies edges" {
	_setup_filter_prod_workspace
	run aube --filter-prod '@scope/api...' run hello
	assert_success
	assert_output --partial "api-ran"
	assert_output --partial "lib-ran"
	refute_output --partial "tooling-ran"
}

@test "aube --filter: graph selector follows devDependencies edges" {
	_setup_filter_prod_workspace
	run aube -F '@scope/api...' run hello
	assert_success
	assert_output --partial "api-ran"
	assert_output --partial "lib-ran"
	assert_output --partial "tooling-ran"
}

@test "aube --filter-prod: dependents walk skips devDependencies edges" {
	_setup_filter_prod_workspace
	# tooling is only pulled in through api's devDependencies, so a
	# prod-only reverse walk from tooling shouldn't reach api.
	run aube --filter-prod '...@scope/tooling' run hello
	assert_success
	assert_output --partial "tooling-ran"
	refute_output --partial "api-ran"
	refute_output --partial "lib-ran"
}

@test "aube --filter-prod: exact-name selector behaves like --filter" {
	_setup_filter_prod_workspace
	run aube --filter-prod '@scope/api' run hello
	assert_success
	assert_output --partial "api-ran"
	refute_output --partial "lib-ran"
	refute_output --partial "tooling-ran"
}

@test "aube --filter-prod: combines with --filter (union)" {
	_setup_filter_prod_workspace
	# --filter-prod 'api...' → api + lib (dev edge skipped)
	# --filter 'tooling'     → tooling
	# union should run all three scripts.
	run aube --filter-prod '@scope/api...' --filter '@scope/tooling' run hello
	assert_success
	assert_output --partial "api-ran"
	assert_output --partial "lib-ran"
	assert_output --partial "tooling-ran"
}
