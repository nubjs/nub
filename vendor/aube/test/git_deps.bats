#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
	# Keep git identity hermetic — the test user may not have a
	# global `user.name` / `user.email` configured, and our clone
	# dance calls `git commit` inside the fixture.
	export GIT_AUTHOR_NAME="aube-tests"
	export GIT_AUTHOR_EMAIL="tests@aube.invalid"
	export GIT_COMMITTER_NAME="aube-tests"
	export GIT_COMMITTER_EMAIL="tests@aube.invalid"
}

teardown() {
	_common_teardown
}

# Build a bare git repo at $1 seeded with a single package.json commit.
# Echoes the commit SHA on stdout so tests can pin to it.
_make_git_repo() {
	local bare="$1" name="$2" version="$3"
	local work
	work="$(temp_make)"
	(
		cd "$work" || exit 1
		git init -q
		git config commit.gpgsign false
		cat >package.json <<EOF
{"name":"$name","version":"$version","main":"index.js"}
EOF
		cat >index.js <<EOF
module.exports = "from $name git";
EOF
		git add -A
		git commit -q -m "init"
	)
	git init -q --bare "$bare"
	(cd "$work" && git push -q "$bare" HEAD:refs/heads/main)
	(cd "$work" && git rev-parse HEAD)
}

# Build a bare git repo whose package.json has a `prepare` script that
# generates a `dist/` directory at build time. Used to verify git deps
# get their `prepare` hook run before the snapshot is taken, matching
# npm/pnpm behavior for source-checkout packages.
_make_git_repo_with_prepare() {
	local bare="$1" name="$2" version="$3"
	local work
	work="$(temp_make)"
	(
		cd "$work" || exit 1
		git init -q
		git config commit.gpgsign false
		cat >package.json <<EOF
{
  "name": "$name",
  "version": "$version",
  "main": "dist/index.js",
  "files": ["dist"],
  "scripts": {
    "prepare": "mkdir -p dist && printf 'module.exports = \"built from %s git\";\\n' $name > dist/index.js"
  }
}
EOF
		# No dist/ committed — it must be produced by `prepare`.
		git add -A
		git commit -q -m "init"
	)
	git init -q --bare "$bare"
	(cd "$work" && git push -q "$bare" HEAD:refs/heads/main)
	(cd "$work" && git rev-parse HEAD)
}

_make_git_repo_with_prepare_dev_dep() {
	local bare="$1" name="$2" version="$3"
	local work
	work="$(temp_make)"
	(
		cd "$work" || exit 1
		git init -q
		git config commit.gpgsign false
		cat >package.json <<EOF
{
  "name": "$name",
  "version": "$version",
  "main": "dist/index.js",
  "files": ["dist"],
  "scripts": {
    "prepare": "node -e \"require('is-number'); require('fs').mkdirSync('dist', {recursive: true}); require('fs').writeFileSync('dist/index.js', 'module.exports = true;')\""
  },
  "devDependencies": {
    "is-number": "7.0.0"
  }
}
EOF
		git add -A
		git commit -q -m "init"
	)
	git init -q --bare "$bare"
	(cd "$work" && git push -q "$bare" HEAD:refs/heads/main)
	(cd "$work" && git rev-parse HEAD)
}

_make_git_repo_with_prepare_build_dep() {
	local bare="$1" name="$2" version="$3"
	local work
	work="$(temp_make)"
	(
		cd "$work" || exit 1
		git init -q
		git config commit.gpgsign false
		cat >package.json <<EOF
{
  "name": "$name",
  "version": "$version",
  "main": "dist/index.js",
  "files": ["dist"],
  "scripts": {
    "prepare": "mkdir -p dist && printf 'module.exports = true;\\n' > dist/index.js"
  },
  "devDependencies": {
    "aube-test-builds-marker": "^1.0.0"
  }
}
EOF
		git add -A
		git commit -q -m "init"
	)
	git init -q --bare "$bare"
	(cd "$work" && git push -q "$bare" HEAD:refs/heads/main)
	(cd "$work" && git rev-parse HEAD)
}

@test "aube install handles git+file:// dep" {
	sha="$(_make_git_repo "$TEST_TEMP_DIR/git-src.git" gitpkg 3.4.5)"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitpkg":"git+file://$TEST_TEMP_DIR/git-src.git"}}
EOF

	run aube install
	assert_success

	assert_file_exists node_modules/gitpkg/package.json
	assert_file_exists node_modules/gitpkg/index.js
	run cat node_modules/gitpkg/package.json
	assert_output --partial '"version":"3.4.5"'

	# Lockfile should record the pinned commit SHA, not the ref.
	run cat aube-lock.yaml
	assert_output --partial "commit: $sha"
	assert_output --partial "type: git"
	assert_output --partial "repo: file://$TEST_TEMP_DIR/git-src.git"
}

# A `packageExtensions` entry that injects a dependency into a *git*
# package must be honored. The non-registry resolve path (git / remote
# tarball / directory) used to skip packageExtensions entirely — only the
# registry path applied them — so a dep injected into a git package (e.g.
# a connector the package `require()`s dynamically at runtime, which must
# land as a sibling in the virtual store) was never resolved or linked.
@test "packageExtensions inject a dependency into a git package" {
	_make_git_repo "$TEST_TEMP_DIR/ext-host.git" exthost 1.0.0 >/dev/null
	conn_sha="$(_make_git_repo "$TEST_TEMP_DIR/ext-conn.git" extconn 2.0.0)"

	mkdir -p app
	cd app
	# The injected connector is itself a git (exotic) dep, so trust
	# exotic transitives — mirrors the real-world config that injects
	# git connectors via packageExtensions.
	cat >.npmrc <<EOF
blockExoticSubdeps=false
EOF
	# exthost declares no deps of its own; extconn is added purely via
	# packageExtensions, exactly like the injected-connector case.
	cat >package.json <<EOF
{
  "name": "app",
  "version": "0.0.0",
  "dependencies": {
    "exthost": "git+file://$TEST_TEMP_DIR/ext-host.git"
  },
  "pnpm": {
    "packageExtensions": {
      "exthost@*": {
        "dependencies": {
          "extconn": "git+file://$TEST_TEMP_DIR/ext-conn.git#$conn_sha"
        }
      }
    }
  }
}
EOF

	run aube install
	assert_success

	# The injected dep is resolved (recorded in the lockfile)...
	run cat aube-lock.yaml
	assert_output --partial "extconn"
	# ...and linked as a sibling inside the git host's own virtual-store
	# node_modules, so the host can require() it at runtime.
	run bash -c 'ls -d node_modules/.aube/exthost@*/node_modules/extconn'
	assert_success
	assert_file_exists "$(ls -d node_modules/.aube/exthost@*/node_modules/extconn)/package.json"
}

@test "aube install handles git ref (#branch) and pins it" {
	sha="$(_make_git_repo "$TEST_TEMP_DIR/git-ref.git" gitref 1.0.0)"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitref":"git+file://$TEST_TEMP_DIR/git-ref.git#main"}}
EOF

	run aube install
	assert_success

	assert_file_exists node_modules/gitref/package.json
	run cat aube-lock.yaml
	# The pinned commit, not the ref name, is what ends up in the lockfile.
	assert_output --partial "commit: $sha"
}

@test "lockfile round-trip for git dep" {
	sha="$(_make_git_repo "$TEST_TEMP_DIR/git-rt.git" gitrt 2.0.0)"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitrt":"git+file://$TEST_TEMP_DIR/git-rt.git"}}
EOF

	run aube install
	assert_success
	first_lockfile="$(cat aube-lock.yaml)"

	# Wipe node_modules and install again from the existing lockfile.
	rm -rf node_modules
	run aube install
	assert_success
	second_lockfile="$(cat aube-lock.yaml)"

	assert_file_exists node_modules/gitrt/package.json
	# Lockfile must be byte-identical on a second install.
	[ "$first_lockfile" = "$second_lockfile" ]
}

@test "aube install honors git resolved URL from package-lock.json" {
	sha="$(_make_git_repo "$TEST_TEMP_DIR/pkg-lock-git.git" pkglockgit 4.5.6)"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"pkglockgit":"git+file://$TEST_TEMP_DIR/pkg-lock-git.git#$sha"}}
EOF
	cat >package-lock.json <<EOF
{
  "name": "app",
  "version": "0.0.0",
  "lockfileVersion": 2,
  "requires": true,
  "packages": {
    "": {
      "name": "app",
      "version": "0.0.0",
      "dependencies": {
        "pkglockgit": "git+file://$TEST_TEMP_DIR/pkg-lock-git.git#$sha"
      }
    },
    "node_modules/pkglockgit": {
      "version": "4.5.6",
      "resolved": "git+file://$TEST_TEMP_DIR/pkg-lock-git.git#$sha"
    }
  }
}
EOF

	run aube -v install
	assert_success
	assert_output --partial "package-lock.json: 1 packages"
	assert_file_exists node_modules/pkglockgit/package.json
	assert_file_exists node_modules/pkglockgit/index.js
	run cat node_modules/pkglockgit/package.json
	assert_output --partial '"version":"4.5.6"'
}

# Build a bare git repo whose tree has a `packages/<sub>/package.json`
# instead of one at the root. Used to exercise the pnpm `&path:/<sub>`
# selector that narrows a git dep to a subdirectory of the clone.
_make_git_repo_with_subpath() {
	local bare="$1" subdir="$2" name="$3" version="$4"
	local work
	work="$(temp_make)"
	(
		cd "$work" || exit 1
		git init -q
		git config commit.gpgsign false
		# Root-level package.json with a name that does NOT match
		# what the consumer asks for — so a wrong implementation
		# (importing the repo root) is detectable as the wrong
		# `index.js` being missing in the consumer's node_modules.
		cat >package.json <<EOF
{"name":"monorepo-root","version":"0.0.0","private":true}
EOF
		mkdir -p "$subdir"
		cat >"$subdir/package.json" <<EOF
{"name":"$name","version":"$version","main":"index.js"}
EOF
		cat >"$subdir/index.js" <<EOF
module.exports = "from $name subdir";
EOF
		git add -A
		git commit -q -m "init"
	)
	git init -q --bare "$bare"
	(cd "$work" && git push -q "$bare" HEAD:refs/heads/main)
	(cd "$work" && git rev-parse HEAD)
}

@test "aube install handles git dep with &path: subpath selector" {
	sha="$(_make_git_repo_with_subpath "$TEST_TEMP_DIR/git-sub.git" packages/foo gitsubpkg 1.2.3)"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitsubpkg":"git+file://$TEST_TEMP_DIR/git-sub.git#main&path:/packages/foo"}}
EOF

	run aube install
	assert_success

	# The dep we get must come from the subdir, not the repo root.
	# Root's package.json has name "monorepo-root" — if aube ignored
	# the &path: selector, the consumer's node_modules entry would
	# be missing index.js (root tree has no index.js).
	assert_file_exists node_modules/gitsubpkg/package.json
	assert_file_exists node_modules/gitsubpkg/index.js
	run cat node_modules/gitsubpkg/package.json
	assert_output --partial '"name":"gitsubpkg"'
	assert_output --partial '"version":"1.2.3"'
	run cat node_modules/gitsubpkg/index.js
	assert_output --partial 'from gitsubpkg subdir'

	# Lockfile pins the commit AND records the subpath so a
	# second install (from lockfile alone) reaches the same dir.
	run cat aube-lock.yaml
	assert_output --partial "commit: $sha"
	assert_output --partial "path: /packages/foo"

	# Round-trip: wipe node_modules and install from the existing
	# lockfile. The subpath must survive parse + re-resolve.
	rm -rf node_modules
	run aube install
	assert_success
	assert_file_exists node_modules/gitsubpkg/index.js
	run cat node_modules/gitsubpkg/package.json
	assert_output --partial '"name":"gitsubpkg"'
}

@test "git dep with prepare script builds before linking" {
	sha="$(_make_git_repo_with_prepare "$TEST_TEMP_DIR/git-prep.git" gitprep 1.2.3)"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitprep":"git+file://$TEST_TEMP_DIR/git-prep.git"}}
EOF

	run aube install
	assert_success

	# The repo commits no `dist/`, only a `prepare` script that
	# generates it. If aube runs `prepare`, `dist/index.js` ends
	# up linked into the consumer's node_modules; if it doesn't,
	# `main` points at a missing file.
	assert_file_exists node_modules/gitprep/package.json
	assert_file_exists node_modules/gitprep/dist/index.js
	run cat node_modules/gitprep/dist/index.js
	assert_output --partial 'built from gitprep git'

	# The `files` allowlist excludes `index.js` at the root, so
	# only `dist/` + `package.json` should make it through the
	# snapshot — matches what `npm pack` would publish.
	run test -e node_modules/gitprep/index.js
	assert_failure

	# The nested install writes `aube-lock.yaml` into the scratch
	# copy of the checkout before we pack it. That lockfile is
	# an implementation detail of the prepare step and must not
	# leak into the consumer's `node_modules/` — `aube pack`
	# treats it the same as pnpm-lock.yaml and excludes it.
	run test -e node_modules/gitprep/aube-lock.yaml
	assert_failure

	run cat aube-lock.yaml
	assert_output --partial "commit: $sha"
}

@test "git dep prepare nested install honors top-level registry override" {
	_make_git_repo_with_prepare_dev_dep "$TEST_TEMP_DIR/git-prep-registry.git" gitprepregistry 1.0.0 >/dev/null

	echo "registry=http://127.0.0.1:1/" >"$HOME/.npmrc"

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitprepregistry":"git+file://$TEST_TEMP_DIR/git-prep-registry.git"}}
EOF

	run aube --registry="$AUBE_TEST_REGISTRY" install
	assert_success
	assert_file_exists node_modules/gitprepregistry/dist/index.js
}

@test "git dep prepare nested install inherits top-level allowBuilds" {
	_make_git_repo_with_prepare_build_dep "$TEST_TEMP_DIR/git-prep-allow-builds.git" gitprepallowbuilds 1.0.0 >/dev/null

	mkdir -p app
	cd app
	cat >package.json <<EOF
{
  "name": "app",
  "version": "0.0.0",
  "dependencies": {
    "gitprepallowbuilds": "git+file://$TEST_TEMP_DIR/git-prep-allow-builds.git"
  },
  "pnpm": {
    "allowBuilds": {
      "aube-test-builds-marker": true
    }
  }
}
EOF

	run env AUBE_STRICT_DEP_BUILDS=true aube --registry="$AUBE_TEST_REGISTRY" install
	assert_success
	assert_file_exists node_modules/gitprepallowbuilds/dist/index.js
}

@test "--ignore-scripts reinstall doesn't inherit prior prepare's build artifacts" {
	_make_git_repo_with_prepare "$TEST_TEMP_DIR/git-prep-contam.git" gitprepcontam 1.0.0 >/dev/null

	# First consumer: regular install. Runs `prepare`, so the
	# shared `git_shallow_clone` cache dir under /tmp gets
	# visited. If prepare were run in-place, the cache would
	# now have `dist/` sitting in it.
	mkdir -p app1
	(
		cd app1
		cat >package.json <<EOF
{"name":"app1","version":"0.0.0","dependencies":{"gitprepcontam":"git+file://$TEST_TEMP_DIR/git-prep-contam.git"}}
EOF
		run aube install
		assert_success
		assert_file_exists node_modules/gitprepcontam/dist/index.js
	)

	# Second consumer: same git dep, but installed with
	# --ignore-scripts. If the cache was mutated by the first
	# install, the fallthrough directory-import path would
	# silently pull in the leftover `dist/` — defeating
	# --ignore-scripts. With the scratch-copy fix the cache
	# stays pristine, so `dist/` must not appear.
	mkdir -p app2
	cd app2
	cat >package.json <<EOF
{"name":"app2","version":"0.0.0","dependencies":{"gitprepcontam":"git+file://$TEST_TEMP_DIR/git-prep-contam.git"}}
EOF
	run aube install --ignore-scripts
	assert_success
	assert_file_exists node_modules/gitprepcontam/package.json
	run test -e node_modules/gitprepcontam/dist/index.js
	assert_failure
}

@test "git dep prepare script is skipped under --ignore-scripts" {
	_make_git_repo_with_prepare "$TEST_TEMP_DIR/git-prep-skip.git" gitprepskip 1.0.0 >/dev/null

	mkdir -p app
	cd app
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitprepskip":"git+file://$TEST_TEMP_DIR/git-prep-skip.git"}}
EOF

	run aube install --ignore-scripts
	assert_success

	# With --ignore-scripts, prepare must not run, so dist/ never
	# gets generated. The raw clone (minus .git) is imported
	# instead — root package.json is still present, but the
	# build artifact is absent.
	assert_file_exists node_modules/gitprepskip/package.json
	run test -e node_modules/gitprepskip/dist/index.js
	assert_failure
}

@test "gitShallowHosts narrow list still installs file:// git deps via full fetch" {
	# `file://` URLs have no hostname, so they never match any entry
	# in `gitShallowHosts`. The setting should force the resolver and
	# the installer down the full-fetch path, which must still
	# produce a working checkout.
	sha="$(_make_git_repo "$TEST_TEMP_DIR/git-shallow-narrow.git" gitshallownarrow 1.2.3)"

	mkdir -p app
	cd app
	# Restrict the shallow list to a host that won't match anything
	# we clone in this test — a single-element list is enough to
	# exercise the parse path and the comparison. Use the kebab-case
	# key here (pnpm's .npmrc convention); the companion camelCase
	# test below covers the other alias.
	cat >.npmrc <<EOF
git-shallow-hosts=example.invalid
EOF
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitshallownarrow":"git+file://$TEST_TEMP_DIR/git-shallow-narrow.git"}}
EOF

	run aube install
	assert_success
	assert_file_exists node_modules/gitshallownarrow/package.json
	run cat node_modules/gitshallownarrow/package.json
	assert_output --partial '"version":"1.2.3"'
}

@test "gitShallowHosts empty list forces full fetch for every git dep" {
	# An empty list means *no* host gets a shallow attempt. Covers
	# the "caller opts out entirely" knob — the install still has to
	# succeed against the same local fixture.
	sha="$(_make_git_repo "$TEST_TEMP_DIR/git-shallow-empty.git" gitshallowempty 4.5.6)"

	mkdir -p app
	cd app
	cat >.npmrc <<EOF
gitShallowHosts=
EOF
	cat >package.json <<EOF
{"name":"app","version":"0.0.0","dependencies":{"gitshallowempty":"git+file://$TEST_TEMP_DIR/git-shallow-empty.git"}}
EOF

	run aube install
	assert_success
	assert_file_exists node_modules/gitshallowempty/package.json
	run cat node_modules/gitshallowempty/package.json
	assert_output --partial '"version":"4.5.6"'
}
