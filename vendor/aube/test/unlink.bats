#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube unlink --help" {
	run aube unlink --help
	assert_success
	assert_output --partial "Unlink"
}

@test "aube unlink <pkg> removes a linked entry from node_modules" {
	# Set up a consumer project
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	# Create and link a local directory
	mkdir -p my-lib
	cat >my-lib/package.json <<'EOF'
{"name": "my-lib", "version": "1.0.0"}
EOF
	run aube link ./my-lib
	assert_success
	run test -L node_modules/my-lib
	assert_success

	# Unlink it
	run aube unlink my-lib
	assert_success
	assert_output --partial "Unlinked my-lib"

	# Verify the symlink is gone
	run test -e node_modules/my-lib
	assert_failure
}

@test "aube unlink <pkg> fails if package is not in node_modules" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF
	mkdir -p node_modules

	run aube unlink nonexistent
	assert_failure
	assert_output --partial "not present in node_modules"
}

@test "aube unlink <scoped-pkg> removes scoped linked entry and cleans up scope dir" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	mkdir -p libs/widgets
	cat >libs/widgets/package.json <<'EOF'
{"name": "@myorg/widgets", "version": "1.0.0"}
EOF
	run aube link ./libs/widgets
	assert_success
	run test -L "node_modules/@myorg/widgets"
	assert_success

	run aube unlink @myorg/widgets
	assert_success

	run test -e "node_modules/@myorg/widgets"
	assert_failure
	# Empty scope dir should be cleaned up
	run test -e "node_modules/@myorg"
	assert_failure
}

@test "aube unlink (no args) removes all linked entries in node_modules" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	mkdir -p lib-a lib-b
	cat >lib-a/package.json <<'EOF'
{"name": "lib-a", "version": "1.0.0"}
EOF
	cat >lib-b/package.json <<'EOF'
{"name": "lib-b", "version": "1.0.0"}
EOF

	run aube link ./lib-a
	assert_success
	run aube link ./lib-b
	assert_success

	run aube unlink
	assert_success
	assert_output --partial "Unlinked 2 packages"

	run test -e node_modules/lib-a
	assert_failure
	run test -e node_modules/lib-b
	assert_failure
}

@test "aube unlink (no args) with no linked packages reports nothing found" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF
	mkdir -p node_modules

	run aube unlink
	assert_success
	assert_output --partial "No linked packages found"
}

@test "aube unlink (no args) with no node_modules is a no-op" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	run aube unlink
	assert_success
	assert_output --partial "No node_modules"
}

@test "aube unlink <pkg> refuses to remove symlinks pointing into .aube" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	# Simulate a pnpm-installed symlink: node_modules/react -> .aube/react@18.0.0/node_modules/react
	mkdir -p "node_modules/.aube/react@18.0.0/node_modules/react"
	echo '{"name":"react","version":"18.0.0"}' \
		>"node_modules/.aube/react@18.0.0/node_modules/react/package.json"
	ln -s ".aube/react@18.0.0/node_modules/react" node_modules/react

	run aube unlink react
	assert_failure
	assert_output --partial "not a linked package"

	# Install symlink should still exist
	run test -L node_modules/react
	assert_success
}

@test "aube unlink (no args) skips symlinks pointing into .aube" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	# Set up a mix: one pnpm install symlink + one user link
	mkdir -p "node_modules/.aube/react@18.0.0/node_modules/react"
	echo '{"name":"react","version":"18.0.0"}' \
		>"node_modules/.aube/react@18.0.0/node_modules/react/package.json"
	ln -s ".aube/react@18.0.0/node_modules/react" node_modules/react

	mkdir -p my-lib
	cat >my-lib/package.json <<'EOF'
{"name": "my-lib", "version": "1.0.0"}
EOF
	run aube link ./my-lib
	assert_success

	run aube unlink
	assert_success
	assert_output --partial "Unlinked 1 package"

	# pnpm install symlink must still exist
	run test -L node_modules/react
	assert_success
	# user link must be gone
	run test -e node_modules/my-lib
	assert_failure
}

@test "aube unlink <pkg> refuses to remove non-symlink entries" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	# Create a real directory in node_modules, not a symlink
	mkdir -p node_modules/real-pkg
	echo '{"name":"real-pkg"}' >node_modules/real-pkg/package.json

	run aube unlink real-pkg
	assert_failure
	assert_output --partial "not a symlink"

	# Real dir should still exist
	run test -d node_modules/real-pkg
	assert_success
}
