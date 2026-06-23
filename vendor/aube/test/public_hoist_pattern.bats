#!/usr/bin/env bats

# `public-hoist-pattern` is the surgical version of `shamefully-hoist`:
# instead of flattening the whole graph, only packages whose names
# match one of the configured globs get a top-level symlink. Frameworks
# like Next.js and Storybook rely on this to find transitive deps
# (eslint, prettier, etc.) from the project root.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "public-hoist-pattern via .npmrc hoists matching transitive deps" {
	cat >package.json <<'JSON'
{
  "name": "public-hoist-npmrc",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	echo 'public-hoist-pattern=*number*' >.npmrc
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd
	# is-number is a transitive of is-odd; the pattern promotes it.
	assert_link_exists node_modules/is-number
}

@test "public-hoist-pattern via CLI hoists matching transitive deps" {
	cat >package.json <<'JSON'
{
  "name": "public-hoist-cli",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	run aube install --public-hoist-pattern '*number*'
	assert_success
	assert_link_exists node_modules/is-odd
	assert_link_exists node_modules/is-number
}

@test "publicHoistPattern via pnpm-workspace.yaml hoists matching transitive deps" {
	cat >package.json <<'JSON'
{
  "name": "public-hoist-yaml",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	cat >pnpm-workspace.yaml <<'YAML'
publicHoistPattern:
  - "*number*"
YAML
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd
	assert_link_exists node_modules/is-number
}

@test "public-hoist-pattern negation cancels a matching positive" {
	cat >package.json <<'JSON'
{
  "name": "public-hoist-negation",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	# `*number*` would promote `is-number`, but the trailing `!is-number`
	# negation cancels it. Exercises the full .npmrc → settings → linker
	# pipeline for negations (unit tests cover the matcher in isolation).
	echo 'public-hoist-pattern=*number*,!is-number' >.npmrc
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd
	assert_not_exists node_modules/is-number
}

@test "public-hoist-pattern leaves non-matching transitives alone" {
	cat >package.json <<'JSON'
{
  "name": "public-hoist-narrow",
  "version": "1.0.0",
  "dependencies": {
    "is-odd": "3.0.1"
  }
}
JSON
	# Pattern matches nothing in the graph — install should behave
	# exactly like the default layout.
	echo 'public-hoist-pattern=*eslint*' >.npmrc
	run aube install
	assert_success
	assert_link_exists node_modules/is-odd
	assert_not_exists node_modules/is-number
}

@test "default public-hoist-pattern leaves transitives private" {
	cat >package.json <<'JSON'
{
  "name": "default-public-hoist-empty",
  "version": "1.0.0",
  "dependencies": {
    "@types/react": "18.3.28"
  }
}
JSON
	# No .npmrc override — pnpm's current default is [].
	run aube install
	assert_success
	assert_link_exists node_modules/@types/react
	assert_not_exists node_modules/@types/prop-types
	assert_not_exists node_modules/csstype
}

@test "explicit empty public-hoist-pattern overrides default" {
	cat >package.json <<'JSON'
{
  "name": "empty-override",
  "version": "1.0.0",
  "dependencies": {
    "@types/react": "18.3.28"
  }
}
JSON
	# Setting an explicit empty pattern in .npmrc replaces the default
	# entirely — no transitive gets hoisted.
	echo 'public-hoist-pattern=' >.npmrc
	run aube install
	assert_success
	assert_link_exists node_modules/@types/react
	assert_not_exists node_modules/@types/prop-types
}
