#!/usr/bin/env bats
#
# pnpm records `pnpmfileChecksum` in pnpm-lock.yaml only when a loaded
# pnpmfile actually exports a `hooks` object (requireHooks gates
# `calculatePnpmfileChecksum` on `entries.some((e) => e.hooks != null)`).
# A pnpmfile that exists but exports no hooks — e.g. an empty
# `.pnpmfile.cjs` — gets no checksum. Stamping one regardless makes pnpm
# abort a frozen install with ERR_PNPM_LOCKFILE_CONFIG_MISMATCH, so the
# gate must look at the export, not at file existence.

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

# Seed the pnpm-format fixture so the writer preserves pnpm-lock.yaml
# (pnpmfileChecksum is a pnpm-only field; aube-lock.yaml never carries it).
_seed_pnpm_project() {
	cp "$PROJECT_ROOT/fixtures/basic/package.json" .
	cp "$PROJECT_ROOT/fixtures/basic/pnpm-lock.yaml" .
}

@test "pnpmfileChecksum: empty .pnpmfile.cjs writes no checksum (--lockfile-only)" {
	_seed_pnpm_project
	# jdx's repro: an empty pnpmfile exports no hooks.
	: >.pnpmfile.cjs

	run aube install --lockfile-only --no-frozen-lockfile
	assert_success
	assert_file_exists pnpm-lock.yaml
	assert [ ! -f aube-lock.yaml ]

	run grep -q pnpmfileChecksum pnpm-lock.yaml
	assert_failure
}

@test "pnpmfileChecksum: a pnpmfile with no hooks export writes no checksum" {
	_seed_pnpm_project
	# Exists and loads, but exports no `hooks` — pnpm omits the checksum.
	cat >.pnpmfile.cjs <<'EOF'
module.exports = { notHooks: true };
EOF

	run aube install --lockfile-only --no-frozen-lockfile
	assert_success
	run grep -q pnpmfileChecksum pnpm-lock.yaml
	assert_failure
}

@test "pnpmfileChecksum: a hooks-bearing pnpmfile writes a checksum (full install)" {
	_seed_pnpm_project
	cat >.pnpmfile.cjs <<'EOF'
module.exports = { hooks: { afterAllResolved: (lockfile) => lockfile } };
EOF

	run aube install --no-frozen-lockfile
	assert_success
	assert_file_exists pnpm-lock.yaml
	run grep -q "pnpmfileChecksum: sha256-" pnpm-lock.yaml
	assert_success
}

@test "pnpmfileChecksum: --ignore-pnpmfile drops the checksum even with hooks" {
	_seed_pnpm_project
	cat >.pnpmfile.cjs <<'EOF'
module.exports = { hooks: { afterAllResolved: (lockfile) => lockfile } };
EOF

	# First install records the checksum.
	run aube install --no-frozen-lockfile
	assert_success
	run grep -q "pnpmfileChecksum: sha256-" pnpm-lock.yaml
	assert_success

	# --ignore-pnpmfile means the pnpmfile does not participate, so the
	# checksum must be cleared (matching pnpm, which would otherwise
	# config-drift on a frozen install).
	run aube install --no-frozen-lockfile --ignore-pnpmfile
	assert_success
	run grep -q pnpmfileChecksum pnpm-lock.yaml
	assert_failure
}
