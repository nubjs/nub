#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_make_local_pkg() {
	local dir="$1" name="$2" version="$3"
	mkdir -p "$dir"
	cat >"$dir/package.json" <<EOF
{"name":"$name","version":"$version","main":"index.js"}
EOF
	cat >"$dir/index.js" <<EOF
module.exports = "from $name";
EOF
}

@test "aube install handles file: directory dep" {
	_make_local_pkg vendor-dir vendor-dir 1.2.3

	mkdir -p app
	cd app
	cat >package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"vendor-dir":"file:../vendor-dir"}}
EOF

	run aube install
	assert_success

	assert_file_exists node_modules/vendor-dir/package.json
	assert_file_exists node_modules/vendor-dir/index.js
	run cat node_modules/vendor-dir/package.json
	assert_output --partial '"version":"1.2.3"'

	# Lockfile should record the canonical `file:` specifier
	run cat aube-lock.yaml
	assert_output --partial 'specifier: file:../vendor-dir'
	assert_output --partial 'vendor-dir@file:../vendor-dir'
}

@test "aube install refreshes a settled file: directory dep after its contents change" {
	_make_local_pkg vendor-dir vendor-dir 1.2.3
	printf 'module.exports = "v1";\n' >vendor-dir/index.js
	printf 'obsolete\n' >vendor-dir/obsolete.txt

	mkdir -p app
	cd app
	cat >package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"vendor-dir":"file:../vendor-dir"}}
EOF

	run env CI=1 aube install --offline
	assert_success
	run env CI=1 aube install --offline
	assert_success
	run env CI=1 aube install --offline
	assert_success
	assert_output --partial "Already up to date"

	# Keep the same byte length so freshness cannot rely on file size.
	printf 'module.exports = "v2";\n' >../vendor-dir/index.js
	rm ../vendor-dir/obsolete.txt
	run env CI=1 aube install --offline
	assert_success
	refute_output --partial "Already up to date"
	run cat node_modules/vendor-dir/index.js
	assert_output 'module.exports = "v2";'
	assert_file_not_exists node_modules/vendor-dir/obsolete.txt
}

@test "aube install handles link: symlink dep" {
	_make_local_pkg vendor-link vendor-link 2.0.0

	mkdir -p app
	cd app
	cat >package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"vendor-link":"link:../vendor-link"}}
EOF

	run aube install
	assert_success

	# link: deps are a direct symlink, not a `.aube/` entry.
	[ -L node_modules/vendor-link ]
	run readlink node_modules/vendor-link
	assert_output "../../vendor-link"
	assert_file_exists node_modules/vendor-link/package.json

	# Editing the target should be visible through the symlink.
	echo '{"name":"vendor-link","version":"2.0.1","main":"index.js"}' >../vendor-link/package.json
	run cat node_modules/vendor-link/package.json
	assert_output --partial '"version":"2.0.1"'
}

@test "aube install handles file: tarball dep" {
	# BSD tar (macOS) has no --transform, so stage the files under an
	# actual `package/` directory before archiving.
	mkdir -p staging/package app
	cat >staging/package/package.json <<'EOF'
{"name":"staged-pkg","version":"3.4.5","main":"index.js"}
EOF
	cat >staging/package/index.js <<'EOF'
module.exports = "from staged-pkg";
EOF
	(cd staging && tar -czf ../app/staged-pkg.tgz package)
	cd app

	cat >package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"staged-pkg":"file:./staged-pkg.tgz"}}
EOF

	run aube install
	assert_success

	assert_file_exists node_modules/staged-pkg/package.json
	run cat node_modules/staged-pkg/package.json
	assert_output --partial '"version":"3.4.5"'
}

@test "excludeLinksFromLockfile omits link: deps from importers on write" {
	# With the flag on, adding a link: dep should leave the lockfile's
	# importers section clean — only the file: entry and any registry
	# deps should appear. The packages/snapshots sections are already
	# link-free unconditionally (pnpm parity), so this exclusively
	# exercises the importer-level filter.
	_make_local_pkg vendor-dir vendor-dir 1.0.0
	_make_local_pkg vendor-link vendor-link 1.0.0

	mkdir -p app
	cd app
	cat >.npmrc <<'RC'
exclude-links-from-lockfile=true
RC
	cat >package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"vendor-dir":"file:../vendor-dir","vendor-link":"link:../vendor-link"}}
EOF
	run aube install
	assert_success

	# The link target is still symlinked into node_modules — the flag
	# is purely a lockfile-serialization knob, not a linker one.
	[ -L node_modules/vendor-link ]
	assert_file_exists node_modules/vendor-dir/package.json

	# Lockfile settings header reflects the choice.
	run grep "excludeLinksFromLockfile:" aube-lock.yaml
	assert_output --partial "true"

	# vendor-link must NOT appear in importers.
	run awk '/^importers:/,/^packages:/' aube-lock.yaml
	refute_output --partial "vendor-link:"
	# vendor-dir (file:) is unaffected and still listed.
	assert_output --partial "vendor-dir:"
}

@test "aube install round-trips file:/link: through the lockfile" {
	_make_local_pkg vendor-dir vendor-dir 1.0.0
	_make_local_pkg vendor-link vendor-link 1.0.0

	mkdir -p app
	cd app
	cat >package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"vendor-dir":"file:../vendor-dir","vendor-link":"link:../vendor-link"}}
EOF

	run aube install
	assert_success

	rm -rf node_modules
	run aube install --frozen-lockfile
	assert_success
	assert_file_exists node_modules/vendor-dir/package.json
	[ -L node_modules/vendor-link ]
	run readlink node_modules/vendor-link
	assert_output "../../vendor-link"
}

@test "aube install handles file:/link: in a workspace importer" {
	# Workspace root + two workspace packages + two external local
	# packages the app depends on via file: / link:.
	_make_local_pkg vendor-dir vendor-dir 9.9.9
	_make_local_pkg vendor-link vendor-link 9.9.9
	printf 'module.exports = "v1";\n' >vendor-dir/index.js

	cat >package.json <<'EOF'
{"name":"ws-root","version":"0.0.0","private":true}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - "packages/*"
EOF
	mkdir -p packages/app
	cat >packages/app/package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"vendor-dir":"file:../../vendor-dir","vendor-link":"link:../../vendor-link"}}
EOF

	run aube install --offline
	assert_success
	run aube install --offline
	assert_success
	run aube install --offline
	assert_success
	assert_output --partial "Already up to date"

	assert_file_exists packages/app/node_modules/vendor-dir/package.json
	run cat packages/app/node_modules/vendor-dir/package.json
	assert_output --partial '"version":"9.9.9"'
	[ -L packages/app/node_modules/vendor-link ]
	# The symlink must actually resolve to the target's package.json
	# — a stale symlink pointing at the wrong base dir would silently
	# pass the `[ -L ]` check above.
	assert_file_exists packages/app/node_modules/vendor-link/package.json
	run cat packages/app/node_modules/vendor-link/package.json
	assert_output --partial '"version":"9.9.9"'

	printf 'module.exports = "v2";\n' >vendor-dir/index.js
	run aube install --offline
	assert_success
	refute_output --partial "Already up to date"
	run cat packages/app/node_modules/vendor-dir/index.js
	assert_output 'module.exports = "v2";'
}

@test "aube install preserves pnpm workspace link targets relative to importer" {
	mkdir -p pkg-a gems/pkg-b-parent/pkg-b
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - "pkg-a"
  - "gems/pkg-b-parent/pkg-b"
EOF
	cat >pkg-a/package.json <<'EOF'
{"name":"pkg-a","version":"0.0.0","dependencies":{"pkg-b":"link:../gems/pkg-b-parent/pkg-b"}}
EOF
	cat >gems/pkg-b-parent/pkg-b/package.json <<'EOF'
{"name":"pkg-b","version":"0.0.0","main":"index.js"}
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .: {}

  pkg-a:
    dependencies:
      pkg-b:
        specifier: link:../gems/pkg-b-parent/pkg-b
        version: link:../gems/pkg-b-parent/pkg-b

  gems/pkg-b-parent/pkg-b: {}
EOF

	run aube install --frozen-lockfile
	assert_success

	[ -L pkg-a/node_modules/pkg-b ]
	run readlink pkg-a/node_modules/pkg-b
	assert_output "../../gems/pkg-b-parent/pkg-b"
	assert_file_exists pkg-a/node_modules/pkg-b/package.json
}

@test "aube install preserves pnpm workspace link targets in hoisted mode" {
	mkdir -p pkg-a gems/pkg-b-parent/pkg-b
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - "pkg-a"
  - "gems/pkg-b-parent/pkg-b"
EOF
	cat >pkg-a/package.json <<'EOF'
{"name":"pkg-a","version":"0.0.0","dependencies":{"pkg-b":"link:../gems/pkg-b-parent/pkg-b"}}
EOF
	cat >gems/pkg-b-parent/pkg-b/package.json <<'EOF'
{"name":"pkg-b","version":"0.0.0","main":"index.js"}
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .: {}

  pkg-a:
    dependencies:
      pkg-b:
        specifier: link:../gems/pkg-b-parent/pkg-b
        version: link:../gems/pkg-b-parent/pkg-b

  gems/pkg-b-parent/pkg-b: {}
EOF

	run aube install --frozen-lockfile --node-linker=hoisted
	assert_success

	[ -L pkg-a/node_modules/pkg-b ]
	run readlink pkg-a/node_modules/pkg-b
	assert_output "../../gems/pkg-b-parent/pkg-b"
	assert_file_exists pkg-a/node_modules/pkg-b/package.json
}

@test "aube install preserves pnpm workspace protocol link targets in hoisted mode" {
	mkdir -p pkg-a pkg-b
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - "pkg-a"
  - "pkg-b"
EOF
	cat >pkg-a/package.json <<'EOF'
{"name":"pkg-a","version":"0.0.0","dependencies":{"pkg-b":"workspace:*"}}
EOF
	cat >pkg-b/package.json <<'EOF'
{"name":"pkg-b","version":"0.0.0","main":"index.js"}
EOF
	cat >pnpm-lock.yaml <<'EOF'
lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .: {}

  pkg-a:
    dependencies:
      pkg-b:
        specifier: workspace:*
        version: link:../pkg-b

  pkg-b: {}
EOF

	run aube install --frozen-lockfile --node-linker=hoisted
	assert_success

	[ -L pkg-a/node_modules/pkg-b ]
	run readlink pkg-a/node_modules/pkg-b
	assert_output "../../pkg-b"
	assert_file_exists pkg-a/node_modules/pkg-b/package.json
}

@test "aube install honors link: paths in pnpm.overrides as project-root-relative" {
	# Workspace consumer pinned `@company/bar@1.2.3` (registry version),
	# but root `pnpm.overrides` rewrites that to `link:./libs/bar`.
	# Without project-root anchoring the resolver would parse `./libs/bar`
	# against the consumer (`libs/foo`) and walk to a phantom `libs/foo/libs/bar`.
	mkdir -p libs/foo libs/bar
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true,"pnpm":{"overrides":{"@company/bar":"link:./libs/bar"}}}
EOF
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - libs/*
EOF
	cat >libs/foo/package.json <<'EOF'
{"name":"@company/foo","version":"0.0.0","dependencies":{"@company/bar":"1.2.3"}}
EOF
	cat >libs/bar/package.json <<'EOF'
{"name":"@company/bar","version":"9.9.9","main":"index.js"}
EOF

	run aube install
	assert_success

	# Symlink lands at the actual on-disk target.
	[ -L libs/foo/node_modules/@company/bar ]
	assert_file_exists libs/foo/node_modules/@company/bar/package.json
	run cat libs/foo/node_modules/@company/bar/package.json
	assert_output --partial '"version":"9.9.9"'

	# Lockfile records the canonical project-root-relative form, not
	# the importer-rebased form.
	run cat aube-lock.yaml
	assert_output --partial 'version: link:./libs/bar'
	refute_output --partial 'libs/foo/libs/bar'
}

@test "aube install: pnpm.overrides redirects a registry parent's transitive to link: (GVS)" {
	# True registry parent + override-rewritten transitive `link:`. The
	# parent goes through the global virtual store, and without the
	# nested-link map threading through `ensure_in_virtual_store` the
	# sibling symlink at `<gvs>/is-odd@.../node_modules/is-number` would
	# dangle into a non-existent `.aube/is-number@link+...` entry. GVS
	# is on by default outside CI (and `_common_setup` clears `CI`), so
	# this exercises the default install path.
	if [ -z "${AUBE_TEST_REGISTRY:-}" ]; then
		skip "AUBE_TEST_REGISTRY not set (Verdaccio not running)"
	fi

	mkdir -p libs/is-number
	cat >libs/is-number/package.json <<'EOF'
{"name":"is-number","version":"9.9.9","main":"index.js"}
EOF
	cat >.npmrc <<EOF
registry=${AUBE_TEST_REGISTRY}
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","dependencies":{"is-odd":"^3.0.0"},"pnpm":{"overrides":{"is-number":"link:./libs/is-number"}}}
EOF

	run aube install
	assert_success

	# Walk through the symlink chain. Per-project entry resolves to
	# either a real dir (per-project mode) or the GVS subdir; either
	# way the sibling `is-number` must point at the on-disk override
	# target (libs/is-number), NOT a phantom `.aube/is-number@link+...`
	# that would dangle.
	local nested
	nested=$(echo node_modules/.aube/is-odd@*/node_modules/is-number)
	[ -L "$nested" ]
	local target
	target=$(readlink "$nested")
	# Stored target must end at libs/is-number (the override target),
	# not at a phantom `.aube/is-number@link+...` entry.
	[[ "$target" == *libs/is-number ]]
	[[ "$target" != *@link+* ]]
	# And must actually resolve through the symlink chain. A
	# tmp→final off-by-one in the GVS materialize would land one dir
	# short on Windows / strict-`..` resolvers and clamp at `/` on
	# POSIX, so chase to the real file.
	assert_file_exists "$nested/package.json"
	run cat "$nested/package.json"
	assert_output --partial '"version":"9.9.9"'
}

@test "aube install lets pnpm.overrides redirect transitive registry deps to link:" {
	# Registry parent → `link:` override. The exotic-subdep guard is on
	# by default, but a root-declared override is an opt-in — without a
	# `range_from_override` short-circuit the guard would block the
	# override before the resolver ever read it.
	mkdir -p libs/bar
	cat >libs/bar/package.json <<'EOF'
{"name":"@company/bar","version":"9.9.9","main":"index.js"}
EOF
	# Vendored registry parent: a tarball whose package.json declares
	# `@company/bar@1.2.3` as a registry dep, which the override redirects
	# to libs/bar. file:./parent.tgz keeps the test offline.
	mkdir -p staging/package
	cat >staging/package/package.json <<'EOF'
{"name":"parent-reg","version":"2.0.0","dependencies":{"@company/bar":"1.2.3"}}
EOF
	(cd staging && tar -czf ../parent-reg.tgz package)
	rm -rf staging

	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","dependencies":{"parent-reg":"file:./parent-reg.tgz"},"pnpm":{"overrides":{"@company/bar":"link:./libs/bar"}}}
EOF

	run aube install
	assert_success

	# The override fires for the transitive — no `BlockedExoticSubdep`.
	# Sibling symlink in the parent's virtual-store node_modules points
	# straight at libs/bar.
	local nested
	nested=$(echo node_modules/.aube/parent-reg@file+*/node_modules/@company/bar)
	[ -L "$nested" ]
	assert_file_exists "$nested/package.json"
	run cat "$nested/package.json"
	assert_output --partial '"version":"9.9.9"'
}

@test "aube install resolves transitive link: against the parent's source root" {
	# A `file:`-linked parent with its own `link:./libs/...` transitive
	# dep. The resolver must anchor `./libs/...` on the parent's source
	# directory, not the importer's, otherwise it bails with "transitive
	# local specifier ... cannot be resolved without the parent package
	# source root".
	mkdir -p parent-pkg/libs/child-link
	cat >parent-pkg/package.json <<'EOF'
{"name":"parent-pkg","version":"1.0.0","dependencies":{"child-link":"link:./libs/child-link"}}
EOF
	cat >parent-pkg/libs/child-link/package.json <<'EOF'
{"name":"child-link","version":"4.5.6","main":"index.js"}
EOF
	cat >parent-pkg/libs/child-link/index.js <<'EOF'
module.exports = "from child-link";
EOF

	mkdir -p app
	cd app
	cat >package.json <<'EOF'
{"name":"app","version":"0.0.0","dependencies":{"parent-pkg":"file:../parent-pkg"}}
EOF

	run aube install
	assert_success

	# parent-pkg is materialized through the virtual store under
	# `.aube/parent-pkg@file+<hash>/node_modules/`. The transitive
	# `child-link` lives as a sibling there — symlinked straight at the
	# parent's on-disk libs/ subdir, bypassing any `.aube/` entry of its
	# own (mirrors how root-level `link:` deps work).
	assert_file_exists node_modules/parent-pkg/package.json
	local nested
	nested=$(echo node_modules/.aube/parent-pkg@file+*/node_modules/child-link)
	[ -L "$nested" ]
	assert_file_exists "$nested/package.json"
	run cat "$nested/package.json"
	assert_output --partial '"version":"4.5.6"'
}
