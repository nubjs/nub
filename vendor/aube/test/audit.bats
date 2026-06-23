#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

_write_clean_fixture() {
	cat >package.json <<-'EOF'
		{
		  "name": "audit-fixture",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
}

@test "aube audit reports no vulnerabilities for a clean tree" {
	_write_clean_fixture
	run aube install
	assert_success

	run aube audit
	assert_success
	assert_output --partial "No known vulnerabilities found"
}

@test "aube audit --json emits an empty object when nothing is vulnerable" {
	_write_clean_fixture
	run aube install
	assert_success

	run aube audit --json
	assert_success
	assert_output --partial "{}"
}

@test "aube audit fails without a lockfile" {
	_write_clean_fixture
	run aube audit
	assert_failure
	assert_output --partial "no lockfile"
}

@test "aube audit --ignore-registry-errors exits 2 with degraded status" {
	_write_clean_fixture
	run aube install
	assert_success

	# Point at a dead port so the bulk POST fails. The flag used to
	# swallow the error and exit 0 with "No known vulnerabilities
	# found", which masked real CVEs in CI when the registry was
	# down. Now it exits 2 and says "audit degraded" so downstream
	# scripts can tell a clean scan from a failed one.
	echo "registry=http://127.0.0.1:1/" >.npmrc
	run aube audit --ignore-registry-errors
	[ "$status" -eq 2 ]
	assert_output --partial "audit degraded"
}

_start_audit_server() {
	# Writes `audit-server.mjs` + `audit-server-port` (and exports
	# `AUDIT_SERVER_PID`). The server responds with a low + high advisory
	# on `is-number`, a high advisory on `is-odd`, and proxies the
	# `is-number` packument so `--fix` / `--ignore-unfixable` can resolve
	# a clean version. `--registry` tests point here while `.npmrc`
	# points at a dead port, so any leakage to the configured registry
	# shows up as a hang or connection error.
	cat >audit-server.mjs <<'NODE'
import http from 'node:http';
import fs from 'node:fs';

const storage = process.env.STORAGE;
const isNumber = JSON.parse(fs.readFileSync(`${storage}/is-number/package.json`, 'utf8'));
const server = http.createServer((req, res) => {
  if (req.method === 'POST' && req.url === '/-/npm/v1/security/advisories/bulk') {
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({
      'is-number': [
        {
          id: 1001,
          severity: 'high',
          title: 'fixture advisory (number-high)',
          vulnerable_versions: '<7.0.0',
          github_advisory_id: 'GHSA-aaaa-bbbb-cccc',
          cves: ['CVE-2099-0001'],
          url: 'https://example.test/advisory/1001'
        },
        {
          id: 1002,
          severity: 'low',
          title: 'fixture advisory (number-low)',
          // Deliberately narrow so `--fix` can still pick 7.0.0: if
          // this range overlapped with number-high (<7.0.0) we'd have
          // no version satisfying "not in either advisory's range".
          vulnerable_versions: '<1.0.0',
          github_advisory_id: 'GHSA-dddd-eeee-ffff',
          cves: [],
          url: 'https://example.test/advisory/1002'
        }
      ],
      'is-odd': [{
        id: 2001,
        severity: 'high',
        title: 'fixture advisory (odd-unfixable)',
        vulnerable_versions: '<9999.0.0',
        github_advisory_id: 'GHSA-zzzz-yyyy-xxxx',
        cves: ['CVE-2099-0002'],
        url: 'https://example.test/advisory/2001'
      }]
    }));
    return;
  }
  if (req.method === 'GET' && req.url === '/is-number') {
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify(isNumber));
    return;
  }
  if (req.method === 'GET' && req.url === '/is-odd') {
    // Every version is vulnerable per the advisory above, so return a
    // minimal packument whose `versions` map stays inside the range.
    res.setHeader('content-type', 'application/json');
    res.end(JSON.stringify({
      name: 'is-odd',
      'dist-tags': { latest: '3.0.1' },
      versions: { '3.0.1': { name: 'is-odd', version: '3.0.1' } },
      time: {}
    }));
    return;
  }
  res.statusCode = 404;
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  fs.writeFileSync('audit-server-port', String(server.address().port));
});
NODE
	STORAGE="$PROJECT_ROOT/test/registry/storage" node audit-server.mjs &
	AUDIT_SERVER_PID=$!
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		[ -f audit-server-port ] && break
		sleep 0.1
	done
}

_stop_audit_server() {
	if [ -n "${AUDIT_SERVER_PID:-}" ]; then
		kill "$AUDIT_SERVER_PID" 2>/dev/null || true
		wait "$AUDIT_SERVER_PID" 2>/dev/null || true
		AUDIT_SERVER_PID=
	fi
}

_install_audit_fixture() {
	cat >package.json <<-'EOF'
		{
		  "name": "audit-flag-fixture",
		  "version": "1.0.0",
		  "dependencies": { "is-odd": "3.0.1" }
		}
	EOF
	run aube install
	assert_success
}

_write_audit_update_fixture() {
	cat >package.json <<-'EOF'
		{
		  "name": "audit-update-fixture",
		  "version": "1.0.0",
		  "dependencies": { "is-number": ">=0.1.0" }
		}
	EOF
	cat >aube-lock.yaml <<-'EOF'
		lockfileVersion: '9.0'

		settings:
		  autoInstallPeers: true
		  excludeLinksFromLockfile: false

		importers:
		  .:
		    dependencies:
		      is-number:
		        specifier: '>=0.1.0'
		        version: 3.0.0

		packages:
		  is-number@3.0.0:
		    resolution: {integrity: sha512-4cboCqIpliH+mAvFNegjZQ4kgKc3ZUhQVr3HvWbSh5q3WH2v82ct+T2Y1hdU5Gdtorx/cLifQjqCbL7bpznLTg==}

		snapshots:
		  is-number@3.0.0: {}
	EOF
}

_write_audit_update_exact_fixture() {
	cat >package.json <<-'EOF'
		{
		  "name": "audit-update-exact-fixture",
		  "version": "1.0.0",
		  "dependencies": { "is-number": "3.0.0" }
		}
	EOF
	cat >aube-lock.yaml <<-'EOF'
		lockfileVersion: '9.0'

		settings:
		  autoInstallPeers: true
		  excludeLinksFromLockfile: false

		importers:
		  .:
		    dependencies:
		      is-number:
		        specifier: 3.0.0
		        version: 3.0.0

		packages:
		  is-number@3.0.0:
		    resolution: {integrity: sha512-4cboCqIpliH+mAvFNegjZQ4kgKc3ZUhQVr3HvWbSh5q3WH2v82ct+T2Y1hdU5Gdtorx/cLifQjqCbL7bpznLTg==}

		snapshots:
		  is-number@3.0.0: {}
	EOF
}

@test "aube audit --fix writes package.json overrides" {
	_install_audit_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	run aube audit --fix
	_stop_audit_server
	assert_success
	assert_output --partial "Updated package.json overrides"

	run node -e 'const p=require("./package.json"); if (p.overrides["is-number"] !== "7.0.0") process.exit(1)'
	assert_success
}

@test "aube audit --fix=override writes package.json overrides" {
	_install_audit_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	run aube audit --fix=override
	_stop_audit_server
	assert_success
	assert_output --partial "Updated package.json overrides"

	run node -e 'const p=require("./package.json"); if (p.overrides["is-number"] !== "7.0.0") process.exit(1)'
	assert_success
}

@test "aube audit -i defaults to override fix mode" {
	_install_audit_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	run aube audit -i
	_stop_audit_server
	assert_success
	assert_output --partial "Updated package.json overrides"

	run node -e 'const p=require("./package.json"); if (p.overrides["is-number"] !== "7.0.0") process.exit(1)'
	assert_success
}

@test "aube audit --fix=update refreshes the lockfile without overrides" {
	_write_audit_update_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	run aube audit --fix=update
	_stop_audit_server
	assert_success
	assert_output --partial "Updated lockfile for 1 package"

	run grep 'is-number@7.0.0' aube-lock.yaml
	assert_success

	run node -e 'const p=require("./package.json"); if (p.overrides) process.exit(1)'
	assert_success
}

@test "aube audit --fix=update keeps exact pins aligned with lockfile specifiers" {
	_write_audit_update_exact_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	run aube audit --fix=update
	_stop_audit_server
	assert_success
	assert_output --partial "Updated lockfile for 1 package"

	run node -e 'const p=require("./package.json"); if (p.dependencies["is-number"] !== "7.0.0") process.exit(1)'
	assert_success

	run grep 'specifier: 7.0.0' aube-lock.yaml
	assert_success
}

@test "aube audit --ignore drops advisories matching the given ID" {
	_install_audit_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	# GHSA match drops the high is-number advisory; the low one
	# (below default threshold) and the odd-unfixable advisory remain.
	run aube audit --ignore GHSA-aaaa-bbbb-cccc
	rc=$status
	_stop_audit_server
	[ "$rc" -eq 1 ]
	refute_output --partial "number-high"
	assert_output --partial "odd-unfixable"
}

@test "aube audit --ignore accepts comma lists and CVE IDs" {
	_install_audit_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	# CVE match drops is-number-high; numeric id drops is-odd advisory.
	# Raise the threshold so the surviving low-severity is-number entry
	# is filtered out — the final report is clean and exit code is 0.
	run aube audit --audit-level moderate --ignore CVE-2099-0001,2001
	rc=$status
	_stop_audit_server
	[ "$rc" -eq 0 ]
	assert_output --partial "No known vulnerabilities found"
}

@test "aube audit --ignore-unfixable drops advisories with no clean version" {
	_install_audit_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	echo "registry=http://127.0.0.1:${port}/" >.npmrc

	# is-odd's advisory covers every published version (<9999.0.0), so
	# `--ignore-unfixable` drops it. is-number's high advisory has a
	# clean 7.0.0, so it stays.
	run aube audit --ignore-unfixable
	rc=$status
	_stop_audit_server
	[ "$rc" -eq 1 ]
	assert_output --partial "number-high"
	refute_output --partial "odd-unfixable"
}

@test "aube audit --registry overrides the .npmrc advisory endpoint" {
	_install_audit_fixture
	_start_audit_server
	port="$(cat audit-server-port)"
	# .npmrc points at a dead port; the flag should win and the
	# advisories still come back from the live server.
	echo "registry=http://127.0.0.1:1/" >.npmrc

	run aube audit --registry "http://127.0.0.1:${port}/"
	rc=$status
	_stop_audit_server
	[ "$rc" -eq 1 ]
	assert_output --partial "number-high"
}
