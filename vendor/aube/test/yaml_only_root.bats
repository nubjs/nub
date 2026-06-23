#!/usr/bin/env bats
#
# Yaml-only workspace root: pnpm-workspace.yaml at the project root with
# no root package.json. Pure-coordinator monorepos (Turborepo defaults
# and several large OSS repos) ship this layout. The five workspace-
# scoped commands — install, list, run -r, query, why — must all work
# from such a root. Single-project commands (add, remove, root-only
# `run <script>`) keep hard-erroring because they need a manifest to
# act on.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Sets up a yaml-only root with two workspace packages, each with their
# own package.json, scripts, and a deliberate dep so query/why have
# something concrete to look at.
_setup_yaml_only_workspace() {
	cat >pnpm-workspace.yaml <<-'YAML'
		packages:
		  - packages/*
	YAML
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<-'JSON'
		{
		  "name": "a",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" },
		  "scripts": { "hello": "echo hello-from-a" }
		}
	JSON
	cat >packages/b/package.json <<-'JSON'
		{
		  "name": "b",
		  "version": "1.0.0",
		  "dependencies": { "is-number": "7.0.0" },
		  "scripts": { "hello": "echo hello-from-b" }
		}
	JSON
}

@test "aube install from yaml-only workspace root installs every member" {
	_setup_yaml_only_workspace
	run aube install
	assert_success
	assert_link_exists packages/a/node_modules/is-odd
	assert_link_exists packages/b/node_modules/is-number
	assert_file_exists aube-lock.yaml
}

@test "aube list -r --depth=-1 from yaml-only root lists every member" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube list -r --depth=-1
	assert_success
	assert_output --partial "a@1.0.0"
	assert_output --partial "b@1.0.0"
}

@test "aube run -r --no-bail from yaml-only root runs the script in every member" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube run -r --no-bail hello
	assert_success
	assert_output --partial "hello-from-a"
	assert_output --partial "hello-from-b"
}

@test "aube query from yaml-only root matches sub-package deps" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube query '[name=is-odd]'
	assert_success
	assert_output --partial "is-odd@3.0.1"
}

@test "aube why from yaml-only root traces a sub-package dep" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube why is-odd
	assert_success
	assert_output --partial "is-odd"
}

# Single-project commands still need a real manifest. They must error
# clearly rather than silently treating the yaml-only root as a project.
@test "aube add from yaml-only root errors clearly" {
	_setup_yaml_only_workspace
	run aube add is-odd
	assert_failure
}

@test "aube run <script> (no -r) from yaml-only root errors clearly" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube run hello
	assert_failure
	assert_output --partial "no package.json found"
}

@test "aube remove from yaml-only root errors clearly" {
	_setup_yaml_only_workspace
	run aube install
	assert_success

	run aube remove is-odd
	assert_failure
}
