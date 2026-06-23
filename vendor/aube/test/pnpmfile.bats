#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "pnpmfile: afterAllResolved hook runs and can mutate packages[].dependencies" {
	cat >package.json <<'EOF'
{
  "name": "test-pnpmfile",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	# afterAllResolved mutates the lockfile to drop is-number from
	# is-odd's dependencies. We then assert the lockfile reflects the
	# mutation.
	cat >.pnpmfile.cjs <<'EOF'
function afterAllResolved(lockfile, context) {
  context.log('hello from pnpmfile');
  for (const key of Object.keys(lockfile.packages)) {
    const pkg = lockfile.packages[key];
    if (pkg.name === 'is-odd') {
      pkg.dependencies = {};
    }
  }
  return lockfile;
}
module.exports = { hooks: { afterAllResolved } };
EOF

	run aube install
	assert_success
	assert_output --partial 'hello from pnpmfile'

	# The snapshot entry for is-odd should be empty, because the hook
	# cleared its dependencies map.
	run grep -A1 '^  is-odd@' aube-lock.yaml
	assert_success
	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | grep 'is-odd@3.0.1'"
	assert_output --partial 'is-odd@3.0.1: {}'
}

@test "pnpmfile: --ignore-pnpmfile skips the hook entirely" {
	cat >package.json <<'EOF'
{
  "name": "test-ignore-pnpmfile",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	# This hook would throw, failing the install if it ran.
	cat >.pnpmfile.cjs <<'EOF'
module.exports = {
  hooks: {
    afterAllResolved() {
      throw new Error('pnpmfile should not have run');
    },
  },
};
EOF

	run aube install --ignore-pnpmfile
	assert_success
}

@test "pnpmfile: .pnpmfile.mjs takes priority over .pnpmfile.cjs" {
	cat >package.json <<'EOF'
{
  "name": "test-pnpmfile-mjs",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	cat >.pnpmfile.cjs <<'EOF'
module.exports = {
  hooks: {
    afterAllResolved() {
      throw new Error('cjs pnpmfile should not have run');
    },
  },
};
EOF

	cat >.pnpmfile.mjs <<'EOF'
export const hooks = {
  afterAllResolved(lockfile, context) {
    context.log('hello from mjs pnpmfile');
    return lockfile;
  },
};
EOF

	run aube install
	assert_success
	assert_output --partial 'hello from mjs pnpmfile'
	refute_output --partial 'cjs pnpmfile should not have run'
}

@test "pnpmfile: .pnpmfile.mjs named hooks win when default has no hooks" {
	cat >package.json <<'EOF'
{
  "name": "test-pnpmfile-mjs-mixed-exports",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	cat >.pnpmfile.mjs <<'EOF'
export const hooks = {
  afterAllResolved(lockfile, context) {
    context.log('named hooks export ran');
    return lockfile;
  },
};
export default { source: 'tooling-only' };
EOF

	run aube install
	assert_success
	assert_output --partial 'default export has no hooks; using named hooks export'
	assert_output --partial 'named hooks export ran'
}

@test "pnpmfile: readPackage hook mutates transitive dependencies before enqueue" {
	cat >package.json <<'EOF'
{
  "name": "test-read-package",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	# readPackage strips is-number off of is-odd before its transitive
	# deps are enqueued — so it should never appear in the lockfile at
	# all, unlike the afterAllResolved path above (which only scrubs
	# the dependency edge but leaves is-number itself in `packages:`).
	cat >.pnpmfile.cjs <<'EOF'
function readPackage(pkg, context) {
  if (pkg.name === 'is-odd') {
    context.log('readPackage saw is-odd');
    pkg.dependencies = {};
  }
  return pkg;
}
module.exports = { hooks: { readPackage } };
EOF

	run aube install
	assert_success
	assert_output --partial 'readPackage saw is-odd'

	# is-odd itself is present, but its dependencies block is empty.
	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | grep 'is-odd@3.0.1'"
	assert_output --partial 'is-odd@3.0.1: {}'
	# is-number must never have been resolved, since the hook removed
	# the edge before transitives were enqueued.
	run grep 'is-number' aube-lock.yaml
	assert_failure
}

@test "pnpmfile: readPackage hook runs from .pnpmfile.mjs" {
	cat >package.json <<'EOF'
{
  "name": "test-read-package-mjs",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	cat >.pnpmfile.mjs <<'EOF'
export default {
  hooks: {
    readPackage(pkg, context) {
      if (pkg.name === 'is-odd') {
        context.log('mjs readPackage saw is-odd');
        pkg.dependencies = {};
      }
      return pkg;
    },
  },
};
EOF

	run aube install
	assert_success
	assert_output --partial 'mjs readPackage saw is-odd'

	run bash -c "awk '/^snapshots:/,0' aube-lock.yaml | grep 'is-odd@3.0.1'"
	assert_output --partial 'is-odd@3.0.1: {}'
	run grep 'is-number' aube-lock.yaml
	assert_failure
}

@test "pnpmfile: --ignore-pnpmfile skips readPackage too" {
	cat >package.json <<'EOF'
{
  "name": "test-ignore-read-package",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	cat >.pnpmfile.cjs <<'EOF'
module.exports = {
  hooks: {
    readPackage() {
      throw new Error('readPackage should not have run');
    },
  },
};
EOF

	run aube install --ignore-pnpmfile
	assert_success
}

@test "pnpmfile: hook errors surface as install failures" {
	cat >package.json <<'EOF'
{
  "name": "test-pnpmfile-err",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF

	cat >.pnpmfile.cjs <<'EOF'
module.exports = {
  hooks: {
    afterAllResolved() {
      throw new Error('boom from pnpmfile');
    },
  },
};
EOF

	run aube install
	assert_failure
	assert_output --partial 'boom from pnpmfile'
}

@test "pnpmfile: pnpm-workspace.yaml pnpmfilePath override loads hook from a custom location" {
	# pnpm v10 lets `pnpmfilePath` in pnpm-workspace.yaml point at a
	# non-default hook file. Put the hook somewhere the default
	# resolver wouldn't find it, and prove the install still loaded
	# it via the workspace override.
	cat >package.json <<'EOF2'
{
  "name": "test-ws-pnpmfile-path",
  "version": "0.0.0",
  "dependencies": {
    "is-odd": "^3.0.1"
  }
}
EOF2

	cat >pnpm-workspace.yaml <<'EOF2'
packages: []
pnpmfilePath: hooks/custom-pnpmfile.cjs
EOF2

	mkdir -p hooks
	cat >hooks/custom-pnpmfile.cjs <<'EOF2'
function afterAllResolved(lockfile, context) {
  context.log('hello from custom pnpmfile path');
  return lockfile;
}
module.exports = { hooks: { afterAllResolved } };
EOF2

	run aube install
	assert_success
	assert_output --partial 'hello from custom pnpmfile path'
}
