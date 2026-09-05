#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# miette hard-wraps its diagnostics, and the wrap point moves with the
# lockfile path's length — macOS `/private/var/folders/...` temp dirs are
# long enough to split a phrase mid-sentence, which is how the first
# revision of these tests failed only on macOS. Match against
# whitespace-squeezed output so a wrapped phrase still counts.
assert_output_unwrapped() {
	local flat
	flat="$(printf '%s' "$output" | tr -s '[:space:]' ' ')"
	if [[ "$flat" != *"$1"* ]]; then
		printf 'expected (ignoring wrapping): %s\nactual output:\n%s\n' "$1" "$output" >&2
		return 1
	fi
}

# aube reads pnpm lockfile version 9 only. Pre-v9 files are valid YAML
# that carries no `importers:`, so without an explicit version guard
# they parse into an empty graph: the install links nothing, writes
# install state, and exits 0 with the declared dependency missing.

@test "pnpm lockfileVersion 6.0 is refused instead of installing nothing" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-pnpm-v6-lock",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^3.0.1" }
		}
	EOF
	cat >pnpm-lock.yaml <<-'EOF'
		lockfileVersion: '6.0'

		dependencies:
		  is-odd:
		    specifier: ^3.0.1
		    version: 3.0.1

		packages:

		  /is-odd@3.0.1:
		    resolution: {integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==}
		    dev: false
	EOF

	run aube install
	assert_failure
	assert_output --partial "ERR_AUBE_UNSUPPORTED_PNPM_LOCKFILE_VERSION"
	assert_output_unwrapped "lockfileVersion 6.0"
	assert_output_unwrapped "npx pnpm@latest install"
	# The failure must be total: nothing linked, no install state left
	# behind that a later run would treat as fresh.
	run test -e node_modules/is-odd
	assert_failure
	run test -e node_modules/.aube-state
	assert_failure
}

@test "pnpm lockfileVersion 5.4 is refused" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-pnpm-v5-lock",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^3.0.1" }
		}
	EOF
	cat >pnpm-lock.yaml <<-'EOF'
		lockfileVersion: 5.4

		specifiers:
		  is-odd: ^3.0.1

		dependencies:
		  is-odd: 3.0.1

		packages:

		  /is-odd/3.0.1:
		    resolution: {integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==}
		    dev: false
	EOF

	run aube install
	assert_failure
	assert_output --partial "ERR_AUBE_UNSUPPORTED_PNPM_LOCKFILE_VERSION"
	assert_output_unwrapped "lockfileVersion 5.4"
}

@test "pnpm lockfileVersion 6.0 install exits 16" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-pnpm-v6-exit",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^3.0.1" }
		}
	EOF
	cat >pnpm-lock.yaml <<-'EOF'
		lockfileVersion: '6.0'

		dependencies:
		  is-odd:
		    specifier: ^3.0.1
		    version: 3.0.1

		packages:

		  /is-odd@3.0.1:
		    resolution: {integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==}
		    dev: false
	EOF

	run aube install
	assert_equal "$status" 16
}

# A v9 header over a pre-v9 body passes the version check, so the
# reader also refuses slash-prefixed package keys with nothing imported
# — the shape that otherwise linked nothing and exited 0.
@test "pnpm v9 header over a pre-v9 body is refused" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-pnpm-mislabeled-lock",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^3.0.1" }
		}
	EOF
	cat >pnpm-lock.yaml <<-'EOF'
		lockfileVersion: '9.0'

		dependencies:
		  is-odd:
		    specifier: ^3.0.1
		    version: 3.0.1

		packages:

		  /is-odd@3.0.1:
		    resolution: {integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==}
		    dev: false
	EOF

	run aube install
	assert_failure
	assert_output --partial "ERR_AUBE_UNSUPPORTED_PNPM_LOCKFILE_VERSION"
	assert_output_unwrapped "pre-v9 pnpm layout"
	run test -e node_modules/is-odd
	assert_failure
}

# A pre-v9 body whose only dep is a local link has no `packages:` block,
# so the root-level dependency block is what gives it away.
@test "pnpm v9 header over a link-only pre-v9 body is refused" {
	mkdir -p linked
	cat >linked/package.json <<-'EOF'
		{ "name": "linked", "version": "1.0.0" }
	EOF
	cat >package.json <<-'EOF'
		{
		  "name": "test-pnpm-legacy-link-lock",
		  "version": "1.0.0",
		  "dependencies": { "linked": "link:./linked" }
		}
	EOF
	cat >pnpm-lock.yaml <<-'EOF'
		lockfileVersion: '9.0'

		dependencies:
		  linked:
		    specifier: link:./linked
		    version: link:./linked
	EOF

	run aube install
	assert_failure
	assert_output --partial "ERR_AUBE_UNSUPPORTED_PNPM_LOCKFILE_VERSION"
	assert_output_unwrapped "root-level \`dependencies:\` block"
	run test -e node_modules/linked
	assert_failure
}

# A non-numeric version component is not read as its leading number:
# `9.invalid` must not pass as version 9.
@test "pnpm malformed lockfileVersion is refused" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-pnpm-malformed-lock",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "^3.0.1" }
		}
	EOF
	cat >pnpm-lock.yaml <<-'EOF'
		lockfileVersion: '9.invalid'

		importers:

		  .:
		    dependencies:
		      is-odd:
		        specifier: ^3.0.1
		        version: 3.0.1
	EOF

	run aube install
	assert_failure
	assert_output --partial "ERR_AUBE_UNSUPPORTED_PNPM_LOCKFILE_VERSION"
}

# Deleting the unreadable lockfile is the documented escape hatch — it
# has to actually work, so the guard must not leave other state behind.
@test "install succeeds after removing a pre-v9 pnpm lockfile" {
	cat >package.json <<-'EOF'
		{
		  "name": "test-pnpm-v6-recover",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	cat >pnpm-lock.yaml <<-'EOF'
		lockfileVersion: '6.0'

		dependencies:
		  is-odd:
		    specifier: 3.0.1
		    version: 3.0.1

		packages:

		  /is-odd@3.0.1:
		    resolution: {integrity: sha512-CQpnWPrDwmP1+SMHXZhtLtJv90yiyVfluGsX5iNCVkrhQtU3TQHsUWPG9wkdk9Lgd5yNpAg9jQEo90CBaXgWMA==}
		    dev: false
	EOF

	run aube install
	assert_failure

	rm pnpm-lock.yaml
	run aube install
	assert_success
	assert_dir_exists node_modules/is-odd
}
