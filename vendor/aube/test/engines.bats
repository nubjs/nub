#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "engines mismatch warns but install succeeds by default" {
	_setup_basic_fixture
	# Rewrite package.json to declare an impossibly-high node requirement.
	cat >package.json <<-'JSON'
		{
		  "name": "engines-warn-test",
		  "version": "0.0.0",
		  "engines": { "node": ">=999.0.0" },
		  "dependencies": {
		    "is-odd": "3.0.1",
		    "is-even": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	assert_output --partial "Unsupported engine"
	assert_output --partial "wanted node >=999.0.0"
}

@test "engine-strict fails the install on root engines mismatch" {
	_setup_basic_fixture
	cat >package.json <<-'JSON'
		{
		  "name": "engines-strict-test",
		  "version": "0.0.0",
		  "engines": { "node": ">=999.0.0" },
		  "dependencies": {
		    "is-odd": "3.0.1",
		    "is-even": "1.0.0"
		  }
		}
	JSON
	echo 'engine-strict=true' >.npmrc
	run aube install --no-frozen-lockfile
	assert_failure
	assert_output --partial "engine-strict"
}

@test "matching engines constraint installs cleanly without warning" {
	_setup_basic_fixture
	cat >package.json <<-'JSON'
		{
		  "name": "engines-ok-test",
		  "version": "0.0.0",
		  "engines": { "node": ">=1.0.0" },
		  "dependencies": {
		    "is-odd": "3.0.1",
		    "is-even": "1.0.0"
		  }
		}
	JSON
	run aube install --no-frozen-lockfile
	assert_success
	refute_output --partial "Unsupported engine"
}

@test "node-version override is honored" {
	_setup_basic_fixture
	cat >package.json <<-'JSON'
		{
		  "name": "engines-override-test",
		  "version": "0.0.0",
		  "engines": { "node": ">=20.0.0" },
		  "dependencies": {
		    "is-odd": "3.0.1",
		    "is-even": "1.0.0"
		  }
		}
	JSON
	cat >.npmrc <<-'RC'
		node-version=14.0.0
		engine-strict=true
	RC
	run aube install --no-frozen-lockfile
	assert_failure
	assert_output --partial "wanted node >=20.0.0, got 14.0.0"
}

@test "engine-strict --prod does not check filtered-out dev deps" {
	# Regression guard: the engines check must run against the
	# post-filter graph so a devDep pinning an incompatible Node version
	# can't block `aube install --prod`. Root + is-odd are compatible;
	# is-even is devDependencies-only so --prod drops it entirely, and
	# the check should succeed even under engine-strict.
	_setup_basic_fixture
	cat >package.json <<-'JSON'
		{
		  "name": "engines-prod-test",
		  "version": "0.0.0",
		  "engines": { "node": ">=1.0.0" },
		  "dependencies": { "is-odd": "3.0.1" },
		  "devDependencies": { "is-even": "1.0.0" }
		}
	JSON
	cat >.npmrc <<-'RC'
		node-version=18.0.0
		engine-strict=true
	RC
	run aube install --no-frozen-lockfile --prod
	assert_success
}
