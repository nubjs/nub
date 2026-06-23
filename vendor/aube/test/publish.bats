#!/usr/bin/env bats
# shellcheck disable=SC2030,SC2031

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	# Safety net: every test in this file that spawns a background
	# mock registry is expected to call `_stop_publish_server`
	# inline, but a failing assertion between _start and _stop would
	# leak the node process and keep the CI shard alive forever (the
	# http.createServer event loop never exits on its own). Always
	# kill here as a backstop.
	_stop_publish_server
	_common_teardown
}

_write_publishable_pkg() {
	cat >package.json <<-'EOF'
		{
		  "name": "publish-smoke",
		  "version": "0.1.0",
		  "main": "index.js",
		  "files": ["index.js"]
		}
	EOF
	cat >index.js <<-'EOF'
		module.exports = 1
	EOF
}

@test "aube publish --dry-run reports the target without uploading" {
	_write_publishable_pkg

	run aube publish --dry-run --registry=https://r.example.com/
	assert_success
	assert_output --partial "publish-smoke@0.1.0"
	assert_output --partial "dry run"
	assert_output --partial "https://r.example.com/publish-smoke"
	assert_output --partial "index.js"
}

@test "aube publish --dry-run URL-encodes scoped names" {
	cat >package.json <<-'EOF'
		{
		  "name": "@aube-fixture/publish",
		  "version": "0.0.1",
		  "files": ["index.js"]
		}
	EOF
	cat >index.js <<'EOF'
module.exports = 0
EOF

	run aube publish --dry-run --registry=https://r.example.com/
	assert_success
	assert_output --partial "@aube-fixture/publish@0.0.1"
	assert_output --partial "https://r.example.com/@aube-fixture%2Fpublish"
}

@test "aube publish errors without an auth token" {
	_write_publishable_pkg

	run aube publish --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no auth token"
}

@test "aube publish uses npm trusted publishing OIDC when no auth token is configured" {
	_write_publishable_pkg

	cat >trusted-publish-server.mjs <<'NODE'
import http from 'node:http';
import fs from 'node:fs';

const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url.startsWith('/gha-oidc')) {
    const url = new URL(req.url, 'http://127.0.0.1');
    fs.writeFileSync('oidc-audience', url.searchParams.get('audience') || '');
    if (req.headers.authorization !== 'Bearer gha-request-token') {
      res.statusCode = 401;
      res.end('{}');
      return;
    }
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ value: 'gha-id-token' }));
    return;
  }
  if (req.method === 'GET' && req.url === '/publish-smoke') {
    res.statusCode = 404;
    res.end('{}');
    return;
  }
  if (req.method === 'POST' && req.url === '/-/npm/v1/oidc/token/exchange/package/publish-smoke') {
    fs.writeFileSync('exchange-auth', req.headers.authorization || '');
    res.statusCode = 201;
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ token_type: 'oidc', token: 'npm-exchange-token' }));
    return;
  }
  if (req.method === 'PUT' && req.url === '/publish-smoke') {
    fs.writeFileSync('put-auth', req.headers.authorization || '');
    req.resume();
    req.on('end', () => {
      res.statusCode = req.headers.authorization === 'Bearer npm-exchange-token' ? 201 : 401;
      res.end('{"ok":true}');
    });
    return;
  }
  res.statusCode = 404;
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync('trusted-publish-server-port', String(server.address().port));
});
NODE
	node trusted-publish-server.mjs &
	PUBLISH_SERVER_PID=$!
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		[ -f trusted-publish-server-port ] && break
		sleep 0.1
	done
	port="$(cat trusted-publish-server-port)"

	run env \
		GITHUB_ACTIONS=true \
		ACTIONS_ID_TOKEN_REQUEST_URL="http://127.0.0.1:${port}/gha-oidc" \
		ACTIONS_ID_TOKEN_REQUEST_TOKEN=gha-request-token \
		aube publish --no-git-checks --registry "http://127.0.0.1:${port}/"
	rc=$status
	_stop_publish_server
	[ "$rc" -eq 0 ]
	assert_output --partial "+ publish-smoke@0.1.0"

	run cat oidc-audience
	assert_success
	assert_output "npm:127.0.0.1"

	run cat exchange-auth
	assert_success
	assert_output "Bearer gha-id-token"

	run cat put-auth
	assert_success
	assert_output "Bearer npm-exchange-token"
}

@test "aube publish falls back to npmrc auth when GitHub OIDC request fails" {
	_write_publishable_pkg
	echo "//127.0.0.1/:_authToken=fallback-token" >.npmrc

	cat >trusted-publish-server.mjs <<'NODE'
import http from 'node:http';
import fs from 'node:fs';

const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url.startsWith('/gha-oidc')) {
    res.statusCode = 500;
    res.end('oidc unavailable');
    return;
  }
  if (req.method === 'GET' && req.url === '/publish-smoke') {
    res.statusCode = 404;
    res.end('{}');
    return;
  }
  if (req.method === 'PUT' && req.url === '/publish-smoke') {
    fs.writeFileSync('put-auth', req.headers.authorization || '');
    req.resume();
    req.on('end', () => {
      res.statusCode = req.headers.authorization === 'Bearer fallback-token' ? 201 : 401;
      res.end('{"ok":true}');
    });
    return;
  }
  res.statusCode = 404;
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync('trusted-publish-server-port', String(server.address().port));
});
NODE
	node trusted-publish-server.mjs &
	PUBLISH_SERVER_PID=$!
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		[ -f trusted-publish-server-port ] && break
		sleep 0.1
	done
	port="$(cat trusted-publish-server-port)"
	echo "//127.0.0.1:${port}/:_authToken=fallback-token" >.npmrc

	run env \
		GITHUB_ACTIONS=true \
		ACTIONS_ID_TOKEN_REQUEST_URL="http://127.0.0.1:${port}/gha-oidc" \
		ACTIONS_ID_TOKEN_REQUEST_TOKEN=gha-request-token \
		aube publish --no-git-checks --registry "http://127.0.0.1:${port}/"
	rc=$status
	_stop_publish_server
	[ "$rc" -eq 0 ]

	run cat put-auth
	assert_success
	assert_output "Bearer fallback-token"
}

@test "aube publish exchanges OIDC token for post-hook package name" {
	_write_publishable_pkg
	cat >rewrite-name.mjs <<'NODE'
import fs from 'node:fs';
const m = JSON.parse(fs.readFileSync('package.json', 'utf8'));
m.name = 'publish-renamed';
fs.writeFileSync('package.json', JSON.stringify(m, null, 2));
NODE
	node -e "const fs=require('fs'); const m=JSON.parse(fs.readFileSync('package.json','utf8')); m.scripts={prepublishOnly:'node ./rewrite-name.mjs'}; fs.writeFileSync('package.json', JSON.stringify(m, null, 2))"

	cat >trusted-publish-server.mjs <<'NODE'
import http from 'node:http';
import fs from 'node:fs';

const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/publish-smoke') {
    res.statusCode = 404;
    res.end('{}');
    return;
  }
  if (req.method === 'POST' && req.url === '/-/npm/v1/oidc/token/exchange/package/publish-renamed') {
    fs.writeFileSync('exchange-auth', req.headers.authorization || '');
    res.statusCode = 201;
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({ token: 'renamed-exchange-token' }));
    return;
  }
  if (req.method === 'PUT' && req.url === '/publish-renamed') {
    fs.writeFileSync('put-auth', req.headers.authorization || '');
    req.resume();
    req.on('end', () => {
      res.statusCode = req.headers.authorization === 'Bearer renamed-exchange-token' ? 201 : 401;
      res.end('{"ok":true}');
    });
    return;
  }
  res.statusCode = 404;
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync('trusted-publish-server-port', String(server.address().port));
});
NODE
	node trusted-publish-server.mjs &
	PUBLISH_SERVER_PID=$!
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		[ -f trusted-publish-server-port ] && break
		sleep 0.1
	done
	port="$(cat trusted-publish-server-port)"

	run env \
		NPM_ID_TOKEN=gha-id-token \
		aube publish --no-git-checks --registry "http://127.0.0.1:${port}/"
	rc=$status
	_stop_publish_server
	[ "$rc" -eq 0 ]
	assert_output --partial "+ publish-renamed@0.1.0"

	run cat exchange-auth
	assert_success
	assert_output "Bearer gha-id-token"

	run cat put-auth
	assert_success
	assert_output "Bearer renamed-exchange-token"
}

@test "aube publish --provenance errors outside an OIDC-capable CI" {
	_write_publishable_pkg

	# Wipe any ambient CI env the BATS runner might have inherited — we're
	# asserting that, with no OIDC provider in sight, the flag fails loud
	# rather than silently publishing unsigned.
	unset GITHUB_ACTIONS ACTIONS_ID_TOKEN_REQUEST_URL ACTIONS_ID_TOKEN_REQUEST_TOKEN
	unset GITLAB_CI BUILDKITE CIRCLECI

	# Seed a fake auth token so we fail on the provenance step, not on
	# the auth lookup that would short-circuit earlier.
	echo "//r.example.com/:_authToken=fake" >.npmrc

	run aube publish --provenance --registry=https://r.example.com/
	assert_failure
	assert_output --partial "--provenance requires an OIDC-capable CI environment"
}

@test "aube publish --dry-run --provenance probes OIDC instead of skipping" {
	_write_publishable_pkg

	# Same ambient-wipe as above — a dry-run that silently succeeds when
	# OIDC is missing would give users a false green light while validating
	# their CI setup, so dry-run mode must still fail on missing creds.
	unset GITHUB_ACTIONS ACTIONS_ID_TOKEN_REQUEST_URL ACTIONS_ID_TOKEN_REQUEST_TOKEN
	unset GITLAB_CI BUILDKITE CIRCLECI

	run aube publish --dry-run --provenance --registry=https://r.example.com/
	assert_failure
	assert_output --partial "--provenance requires an OIDC-capable CI environment"
}

_setup_workspace_fixture() {
	cp -r "$PROJECT_ROOT/fixtures/workspace/"* .
}

@test "aube publish -r --dry-run fans out across the workspace" {
	_setup_workspace_fixture

	run aube publish -r --dry-run --registry=https://r.example.com/
	assert_success
	# Both non-private workspace packages appear in the dry-run output.
	assert_output --partial "@test/lib@1.0.0"
	assert_output --partial "@test/app@1.0.0"
	# The private workspace-root `aube-test-workspace` must not publish.
	refute_output --partial "aube-test-workspace"
}

@test "aube publish -F selects a single workspace package" {
	_setup_workspace_fixture

	run aube publish -F @test/lib --dry-run --registry=https://r.example.com/
	assert_success
	assert_output --partial "@test/lib@1.0.0"
	refute_output --partial "@test/app"
}

@test "aube publish -r --dry-run --json tags every outcome with a status" {
	_setup_workspace_fixture

	run bash -c "aube publish -r --dry-run --json --registry=https://r.example.com/ | jq -r '.[] | .name + \" \" + .status' | sort"
	assert_success
	assert_line "@test/app dry-run"
	assert_line "@test/lib dry-run"
}

@test "aube publish -F errors cleanly when nothing matches" {
	_setup_workspace_fixture

	run aube publish -F @test/nope --dry-run --registry=https://r.example.com/
	assert_failure
	assert_output --partial "did not match"
}

@test "aube publish -r errors outside a workspace" {
	_write_publishable_pkg

	run aube publish -r --dry-run --registry=https://r.example.com/
	assert_failure
	assert_output --partial "no workspace packages"
}

@test "aube publish -r skips packages marked private" {
	# Build an inline workspace with one publishable package and one
	# private one, so the private-skip logic gets exercised on an
	# actual `packages:` member (the shared fixture's private root
	# isn't a workspace member, so it alone doesn't cover this path).
	cat >pnpm-workspace.yaml <<-'EOF'
		packages:
		  - packages/*
	EOF
	cat >package.json <<-'EOF'
		{"name":"root","version":"0.0.0","private":true}
	EOF
	mkdir -p packages/pub packages/priv
	cat >packages/pub/package.json <<-'EOF'
		{"name":"pub-pkg","version":"0.1.0","main":"index.js","files":["index.js"]}
	EOF
	echo "module.exports = 1" >packages/pub/index.js
	cat >packages/priv/package.json <<-'EOF'
		{"name":"priv-pkg","version":"0.1.0","private":true,"main":"index.js","files":["index.js"]}
	EOF
	echo "module.exports = 1" >packages/priv/index.js

	run aube publish -r --dry-run --registry=https://r.example.com/
	assert_success
	assert_output --partial "pub-pkg@0.1.0"
	refute_output --partial "priv-pkg"
}

_start_publish_server() {
	# Starts a minimal mock registry that reports `publish-smoke@0.1.0`
	# already exists (GET returns a packument with that version) and
	# accepts any PUT. Writes `publish-server-port` and exports
	# `PUBLISH_SERVER_PID`. The PUT handler also writes
	# `publish-server-put.log` so tests can assert whether a request
	# actually reached it.
	cat >publish-server.mjs <<'NODE'
import http from 'node:http';
import fs from 'node:fs';

const existing = {
  name: 'publish-smoke',
  'dist-tags': { latest: '0.1.0' },
  versions: { '0.1.0': { name: 'publish-smoke', version: '0.1.0' } },
};
const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/publish-smoke') {
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(existing));
    return;
  }
  if (req.method === 'PUT' && req.url === '/publish-smoke') {
    let size = 0;
    req.on('data', (c) => { size += c.length; });
    req.on('end', () => {
      fs.appendFileSync('publish-server-put.log', `${req.url} ${size}\n`);
      res.statusCode = 201;
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({ ok: true, id: 'publish-smoke' }));
    });
    return;
  }
  res.statusCode = 404;
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync('publish-server-port', String(server.address().port));
});
NODE
	node publish-server.mjs &
	PUBLISH_SERVER_PID=$!
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		[ -f publish-server-port ] && break
		sleep 0.1
	done
}

_stop_publish_server() {
	if [ -n "${PUBLISH_SERVER_PID:-}" ]; then
		kill "$PUBLISH_SERVER_PID" 2>/dev/null || true
		wait "$PUBLISH_SERVER_PID" 2>/dev/null || true
		PUBLISH_SERVER_PID=
	fi
}

@test "aube publish refuses to re-publish a version already on the registry" {
	_write_publishable_pkg
	_start_publish_server
	port="$(cat publish-server-port)"
	echo "//127.0.0.1:${port}/:_authToken=fake" >.npmrc

	run aube publish --no-git-checks --registry "http://127.0.0.1:${port}/"
	rc=$status
	_stop_publish_server
	[ "$rc" -ne 0 ]
	assert_output --partial "publish-smoke@0.1.0 is already on"
	assert_output --partial "--force"
	# The pre-flight must short-circuit before the PUT; the mock
	# server only logs PUTs to this file.
	[ ! -s publish-server-put.log ] || {
		echo "unexpected PUT: $(cat publish-server-put.log)" >&2
		false
	}
}

@test "aube publish --force re-publishes past the existence check" {
	_write_publishable_pkg
	_start_publish_server
	port="$(cat publish-server-port)"
	echo "//127.0.0.1:${port}/:_authToken=fake" >.npmrc

	run aube publish --force --no-git-checks --registry "http://127.0.0.1:${port}/"
	rc=$status
	_stop_publish_server
	[ "$rc" -eq 0 ]
	# A real PUT went through to the mock.
	run grep -c "^/publish-smoke " publish-server-put.log
	assert_success
	[ "$output" = "1" ]
}

@test "aube publish --dry-run runs prepublishOnly, prepublish, prepack, prepare, postpack" {
	cat >package.json <<-'EOF'
		{
		  "name": "publish-hooks",
		  "version": "0.1.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "scripts": {
		    "prepublishOnly": "echo prepublishOnly >>$HOOK_LOG",
		    "prepublish": "echo prepublish >>$HOOK_LOG",
		    "prepack": "echo prepack >>$HOOK_LOG",
		    "prepare": "echo prepare >>$HOOK_LOG",
		    "postpack": "echo postpack >>$HOOK_LOG",
		    "publish": "echo publish >>$HOOK_LOG",
		    "postpublish": "echo postpublish >>$HOOK_LOG"
		  }
		}
	EOF
	echo "module.exports = 1" >index.js

	export HOOK_LOG="$PWD/hooks.log"
	: >"$HOOK_LOG"

	run aube publish --dry-run --registry=https://r.example.com/
	assert_success

	# Dry-run: pre-pack chain fires, post-upload hooks don't.
	run cat "$HOOK_LOG"
	assert_success
	assert_line --index 0 "prepublishOnly"
	assert_line --index 1 "prepublish"
	assert_line --index 2 "prepack"
	assert_line --index 3 "prepare"
	assert_line --index 4 "postpack"
	refute_line "publish"
	refute_line "postpublish"
}

@test "aube publish --dry-run --ignore-scripts skips lifecycle hooks" {
	cat >package.json <<-'EOF'
		{
		  "name": "publish-hooks",
		  "version": "0.1.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "scripts": {
		    "prepublishOnly": "echo prepublishOnly >>$HOOK_LOG",
		    "prepack": "echo prepack >>$HOOK_LOG",
		    "prepare": "echo prepare >>$HOOK_LOG",
		    "postpack": "echo postpack >>$HOOK_LOG"
		  }
		}
	EOF
	echo "module.exports = 1" >index.js

	export HOOK_LOG="$PWD/hooks.log"
	: >"$HOOK_LOG"

	run aube publish --dry-run --ignore-scripts --registry=https://r.example.com/
	assert_success

	run cat "$HOOK_LOG"
	assert_success
	assert_output ""
}

@test "aube publish --dry-run aborts when prepublishOnly fails" {
	cat >package.json <<-'EOF'
		{
		  "name": "publish-hooks",
		  "version": "0.1.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "scripts": {
		    "prepublishOnly": "exit 5"
		  }
		}
	EOF
	echo "module.exports = 1" >index.js

	run aube publish --dry-run --registry=https://r.example.com/
	assert_failure
}

@test "aube publish re-reads package.json after pre-pack hooks so registry metadata matches tarball" {
	# Regression test for Cursor Bugbot issue: if a `prepublishOnly`
	# script mutates package.json (e.g. stamping a git SHA into the
	# version, stripping devDependencies), `build_publish_body` must
	# see the post-hook manifest so `versions.<v>` metadata agrees
	# with what's in the tarball. Previously we serialized the
	# pre-hook snapshot and consumers saw a mismatch.
	cat >package.json <<-'EOF'
		{
		  "name": "publish-mutated",
		  "version": "0.1.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "devDependencies": {
		    "should-be-stripped": "1.0.0"
		  },
		  "scripts": {
		    "prepublishOnly": "node ./rewrite.mjs"
		  }
		}
	EOF
	echo "module.exports = 1" >index.js
	cat >rewrite.mjs <<'NODE'
import fs from 'node:fs';
const m = JSON.parse(fs.readFileSync('package.json', 'utf8'));
delete m.devDependencies;
m.publishedBy = 'prepublishOnly';
fs.writeFileSync('package.json', JSON.stringify(m, null, 2));
NODE

	cat >record-server.mjs <<'NODE'
import http from 'node:http';
import fs from 'node:fs';

const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/publish-mutated') {
    res.statusCode = 404;
    res.end('{}');
    return;
  }
  if (req.method === 'PUT' && req.url === '/publish-mutated') {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      fs.writeFileSync('put-body.json', Buffer.concat(chunks));
      res.statusCode = 201;
      res.end('{"ok":true}');
    });
    return;
  }
  res.statusCode = 404;
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync('record-server-port', String(server.address().port));
});
NODE
	# Use PUBLISH_SERVER_PID so teardown's `_stop_publish_server`
	# safety net catches it if the test aborts early.
	node record-server.mjs &
	PUBLISH_SERVER_PID=$!
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		[ -f record-server-port ] && break
		sleep 0.1
	done
	port="$(cat record-server-port)"
	echo "//127.0.0.1:${port}/:_authToken=fake" >.npmrc

	run aube publish --no-git-checks --registry "http://127.0.0.1:${port}/"
	rc=$status
	_stop_publish_server
	[ "$rc" -eq 0 ]

	# The PUT body's `versions["0.1.0"]` must reflect the post-hook
	# manifest — `devDependencies` stripped, `publishedBy` added.
	run jq -r '.versions."0.1.0".publishedBy' put-body.json
	assert_success
	assert_output "prepublishOnly"

	run jq '.versions."0.1.0".devDependencies' put-body.json
	assert_success
	assert_output "null"
}

@test "aube publish --dry-run --json reports post-hook version when prepublishOnly bumps version" {
	# Regression: PublishOutcome captured `name`/`version` from the
	# pre-hook manifest, so if `prepublishOnly` stamped a git SHA into
	# the version, the tarball (and registry) would carry the new
	# version but the --json / user-facing line still reported the old
	# one. Outcome now pulls from the built archive, which reads
	# package.json fresh after hooks.
	cat >package.json <<-'EOF'
		{
		  "name": "publish-outcome-version",
		  "version": "0.1.0",
		  "main": "index.js",
		  "files": ["index.js"],
		  "scripts": {
		    "prepublishOnly": "node ./bump.mjs"
		  }
		}
	EOF
	echo "module.exports = 1" >index.js
	cat >bump.mjs <<'NODE'
import fs from 'node:fs';
const m = JSON.parse(fs.readFileSync('package.json', 'utf8'));
m.version = '0.1.0-sha.abc123';
fs.writeFileSync('package.json', JSON.stringify(m, null, 2));
NODE

	run bash -c "aube publish --dry-run --json --registry=https://r.example.com/ | jq -r '.version'"
	assert_success
	assert_output "0.1.0-sha.abc123"
}

@test "aube publish --dry-run --json emits a pnpm-compatible object" {
	_write_publishable_pkg

	run aube publish --dry-run --json --registry=https://r.example.com/
	assert_success
	# Shape matches `npm publish --json` / `pnpm publish --json`: a
	# single object with name/version/filename/files.
	run bash -c "aube publish --dry-run --json --registry=https://r.example.com/ | jq -r '.name + \"@\" + .version'"
	assert_success
	assert_output "publish-smoke@0.1.0"
	run bash -c "aube publish --dry-run --json --registry=https://r.example.com/ | jq -r '.filename'"
	assert_success
	assert_output "publish-smoke-0.1.0.tgz"
	run bash -c "aube publish --dry-run --json --registry=https://r.example.com/ | jq -r '.files[].path' | sort"
	assert_success
	assert_line "index.js"
	assert_line "package.json"
}
