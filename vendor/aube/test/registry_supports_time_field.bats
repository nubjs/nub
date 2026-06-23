#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# `registrySupportsTimeField=true` tells aube the registry ships
# `time` in its abbreviated (corgi) packument, so it can skip the
# separate full-packument fetch under `resolutionMode=time-based` and
# keep the cheaper corgi path on the hot loop. The fixture Verdaccio
# *does* ship `time` in corgi, so a `time:` block still shows up in
# the lockfile on both the fast and slow paths — end-to-end parity is
# what we're asserting, not the request shape.

@test "registrySupportsTimeField=true still produces time: under resolution-mode=time-based" {
	_setup_basic_fixture
	rm aube-lock.yaml
	cat >>.npmrc <<-'EOF'

		registry-supports-time-field=true
	EOF
	run aube install --resolution-mode=time-based
	assert_success
	assert_file_exists aube-lock.yaml
	run grep -E '^time:' aube-lock.yaml
	assert_success
	run grep -E "[a-z0-9@/._-]+@[0-9]+\\.[0-9]+\\.[0-9]+.*: ['\"]?[0-9]{4}-" aube-lock.yaml
	assert_success
}

@test "registrySupportsTimeField=true is accepted with default resolution mode" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-registry-time-default-mode",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >>.npmrc <<-'EOF'

		registry-supports-time-field=true
	EOF
	run aube install --no-frozen-lockfile
	assert_success
	assert_dir_exists node_modules/is-odd
}

@test "registrySupportsTimeField camelCase alias in .npmrc is honored" {
	_setup_basic_fixture
	rm aube-lock.yaml
	cat >>.npmrc <<-'EOF'

		registrySupportsTimeField=true
	EOF
	run aube install --resolution-mode=time-based
	assert_success
	run grep -E '^time:' aube-lock.yaml
	assert_success
}
