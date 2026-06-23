#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube link --help" {
	run aube link --help
	assert_success
	assert_output --partial "Link a local package"
}

@test "aube ln --help (alias)" {
	run aube ln --help
	assert_success
	assert_output --partial "Link a local package"
}

@test "aube link registers current package globally" {
	cat >package.json <<'EOF'
{"name": "my-test-lib", "version": "1.0.0"}
EOF

	run aube link
	assert_success
	assert_output --partial "Linked"

	# Verify the global symlink exists and points to cwd
	run test -L "$HOME/.cache/aube/global-links/my-test-lib"
	assert_success
	run readlink "$HOME/.cache/aube/global-links/my-test-lib"
	assert_output "$(pwd -P)"
}

@test "aube link <pkg> links globally-registered package into node_modules" {
	# First register a package globally
	mkdir -p my-lib
	cat >my-lib/package.json <<'EOF'
{"name": "my-lib", "version": "2.0.0"}
EOF
	(cd my-lib && aube link)

	# Now link it into the current project
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	run aube link my-lib
	assert_success
	assert_output --partial "Linked"

	# Verify symlink in node_modules
	run test -L node_modules/my-lib
	assert_success
	run readlink node_modules/my-lib
	assert_output "$(cd my-lib && pwd -P)"
}

@test "aube link <dir> links a local directory into node_modules" {
	mkdir -p libs/utils
	cat >libs/utils/package.json <<'EOF'
{"name": "utils", "version": "0.1.0"}
EOF

	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	run aube link ./libs/utils
	assert_success
	assert_output --partial "Linked"

	# Verify symlink in node_modules
	run test -L node_modules/utils
	assert_success
	run readlink node_modules/utils
	assert_output "$(cd libs/utils && pwd -P)"
}

@test "aube link fails without package.json" {
	run aube link
	assert_failure
	assert_output --partial "package.json"
}

@test "aube link <pkg> fails if not linked globally" {
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	run aube link nonexistent-pkg
	assert_failure
	assert_output --partial "not linked globally"
}

@test "aube link <dir> fails if target has no package.json" {
	mkdir -p empty-dir

	run aube link ./empty-dir
	assert_failure
	assert_output --partial "package.json"
}

@test "aube link with scoped package" {
	cat >package.json <<'EOF'
{"name": "@myorg/my-lib", "version": "1.0.0"}
EOF

	run aube link
	assert_success

	# Verify the scoped global symlink
	run test -L "$HOME/.cache/aube/global-links/@myorg/my-lib"
	assert_success
}

@test "aube link <scoped-pkg> links globally-registered scoped package into node_modules" {
	# First register a scoped package globally
	mkdir -p scoped-lib
	cat >scoped-lib/package.json <<'EOF'
{"name": "@myorg/my-scoped-lib", "version": "1.0.0"}
EOF
	(cd scoped-lib && aube link)

	# Now link it into the current project by name
	cat >package.json <<'EOF'
{"name": "consumer", "version": "1.0.0"}
EOF

	run aube link @myorg/my-scoped-lib
	assert_success
	assert_output --partial "Linked"

	# Verify scoped symlink in node_modules
	run test -L "node_modules/@myorg/my-scoped-lib"
	assert_success
	run readlink "node_modules/@myorg/my-scoped-lib"
	assert_output "$(cd scoped-lib && pwd -P)"
}

@test "aube link <dir> with scoped package" {
	mkdir -p libs/scoped
	cat >libs/scoped/package.json <<'EOF'
{"name": "@myorg/widgets", "version": "3.0.0"}
EOF

	cat >package.json <<'EOF'
{"name": "app", "version": "1.0.0"}
EOF

	run aube link ./libs/scoped
	assert_success

	# Verify scoped symlink in node_modules
	run test -L "node_modules/@myorg/widgets"
	assert_success
}

@test "aube link overwrites existing link" {
	# Create first version
	mkdir -p lib-v1
	cat >lib-v1/package.json <<'EOF'
{"name": "my-lib", "version": "1.0.0"}
EOF
	(cd lib-v1 && aube link)

	# Create second version and re-link
	mkdir -p lib-v2
	cat >lib-v2/package.json <<'EOF'
{"name": "my-lib", "version": "2.0.0"}
EOF
	(cd lib-v2 && aube link)

	# Global link should now point to v2
	run readlink "$HOME/.cache/aube/global-links/my-lib"
	assert_output "$(cd lib-v2 && pwd -P)"
}

@test "aube link respects the global -C/--dir flag" {
	# Create a target package in a subdir; from a different cwd, point
	# `-C` at it and confirm the global registration uses that dir.
	mkdir -p libs/widget
	cat >libs/widget/package.json <<'EOF2'
{"name": "widget", "version": "1.0.0"}
EOF2

	# Run from the temp dir root so the chdir is observable.
	run aube -C libs/widget link
	assert_success

	# The global symlink should point at the chdir target, not the cwd.
	run readlink "$HOME/.cache/aube/global-links/widget"
	assert_output "$(cd libs/widget && pwd -P)"
}
