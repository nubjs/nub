#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_stop_request_sentinel
	_common_teardown
}

_start_request_sentinel() {
	REQUEST_SENTINEL_PORT_FILE="$BATS_TEST_TMPDIR/registry-request-sentinel.port"
	REQUEST_SENTINEL_HIT_FILE="$BATS_TEST_TMPDIR/registry-request-sentinel.hit"
	rm -f "$REQUEST_SENTINEL_PORT_FILE" "$REQUEST_SENTINEL_HIT_FILE"
	node -e 'const fs = require("fs"); const http = require("http"); const [portFile, hitFile] = process.argv.slice(1); const server = http.createServer((_req, res) => { fs.writeFileSync(hitFile, "hit"); res.statusCode = 500; res.end(); }); server.listen(0, "127.0.0.1", () => fs.writeFileSync(portFile, String(server.address().port)));' "$REQUEST_SENTINEL_PORT_FILE" "$REQUEST_SENTINEL_HIT_FILE" 3>&- &
	REQUEST_SENTINEL_PID=$!

	local tries=40
	while [ "$tries" -gt 0 ]; do
		if [ -s "$REQUEST_SENTINEL_PORT_FILE" ]; then
			REQUEST_SENTINEL_URL="http://127.0.0.1:$(cat "$REQUEST_SENTINEL_PORT_FILE")/"
			return 0
		fi
		sleep 0.05
		tries=$((tries - 1))
	done

	echo "registry request sentinel failed to start" >&2
	return 1
}

_stop_request_sentinel() {
	if [ -n "${REQUEST_SENTINEL_PID:-}" ]; then
		kill "$REQUEST_SENTINEL_PID" 2>/dev/null || true
		wait "$REQUEST_SENTINEL_PID" 2>/dev/null || true
		unset REQUEST_SENTINEL_PID REQUEST_SENTINEL_PORT_FILE REQUEST_SENTINEL_HIT_FILE REQUEST_SENTINEL_URL
	fi
}

@test "aube ci installs from a committed lockfile" {
	_setup_basic_fixture
	run aube ci
	assert_success
	assert_file_exists node_modules/is-odd/package.json
	assert_file_exists aube-lock.yaml
}

@test "aube ci deletes existing node_modules first" {
	_setup_basic_fixture
	# Seed an "old" node_modules with a stale sentinel file
	mkdir -p node_modules
	touch node_modules/.stale-sentinel
	run aube ci
	assert_success
	# Sentinel should be gone — ci deletes node_modules before installing
	assert [ ! -e node_modules/.stale-sentinel ]
	# Fresh install artifacts should be in place
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube ci rejects invalid registry before cleaning node_modules" {
	_setup_basic_fixture
	mkdir -p node_modules
	touch node_modules/.must-survive
	printf '%s\n' 'registry=ftp://default.invalid/' >.npmrc

	run aube ci
	assert_failure
	assert_output --partial 'ERR_AUBE_INVALID_REGISTRY_URL'
	assert_file_exists node_modules/.must-survive
}

@test "aube ci validates an ancestor npmrc before descendant cleanup or requests" {
	_setup_basic_fixture
	root="$PWD"
	mkdir -p nested node_modules
	touch node_modules/.must-survive
	_start_request_sentinel
	cat >.npmrc <<-EOF
		registry=$REQUEST_SENTINEL_URL
		@blocked:registry=ftp://invalid.example/
	EOF

	cd nested
	run aube ci
	assert_failure
	assert_output --partial 'ERR_AUBE_INVALID_REGISTRY_URL'
	assert_file_exists "$root/node_modules/.must-survive"
	assert_file_not_exists "$REQUEST_SENTINEL_HIT_FILE"
}

@test "aube ci from a descendant uses ancestor npmrc and project root" {
	_setup_basic_fixture
	root="$PWD"
	mkdir -p nested ci_modules
	touch ci_modules/.stale-sentinel
	printf '%s\n' 'modules-dir=ci_modules' >.npmrc

	cd nested
	run aube ci
	assert_success
	assert_file_not_exists "$root/ci_modules/.stale-sentinel"
	assert_file_exists "$root/ci_modules/is-odd/package.json"
}

@test "aube ci rejects blank classic Yarn routes before cleanup or requests" {
	# Classic `.yarnrc` is a Nub compat surface; standalone aube intentionally
	# does not read it. The Nub-adapted harness sets this marker for this proof.
	[ "${NUB_AUBE_BATS:-}" = 1 ] || skip "requires Nub-adapted Bats harness"

	_setup_basic_fixture
	node -e 'let p=require("./package.json"); p.packageManager="yarn@1.22.22"; require("fs").writeFileSync("package.json", JSON.stringify(p))'
	mkdir -p node_modules
	touch node_modules/.must-survive
	_start_request_sentinel
	# A dropped classic route would inherit this valid lower route, clean the
	# tree, and request the sentinel. Project `.yarnrc` must instead reach the
	# shared registry preflight first.
	printf 'registry=%s\n' "$REQUEST_SENTINEL_URL" >.npmrc

	for route in default scoped; do
		case "$route" in
			default) printf 'registry ""\n' >.yarnrc ;;
			scoped) printf '@blocked:registry ""\n' >.yarnrc ;;
		esac

		run aube ci
		assert_failure
		assert_output --partial 'invalid registry URL'
		assert_file_exists node_modules/.must-survive
		assert_file_not_exists node_modules/.aube-state
		assert_file_not_exists node_modules/.store
		[ ! -e "$REQUEST_SENTINEL_HIT_FILE" ]
	done
}

@test "aube ci errors when no lockfile is present" {
	echo '{"name":"no-lockfile","version":"1.0.0","dependencies":{"is-odd":"^3.0.1"}}' >package.json
	run aube ci
	assert_failure
	assert_output --partial "no lockfile found and --frozen-lockfile is set"
}

@test "aube ci errors when lockfile drifts from package.json" {
	_setup_basic_fixture
	# Mutate package.json so the lockfile is stale
	node -e '
		const fs = require("fs");
		const pkg = JSON.parse(fs.readFileSync("package.json"));
		pkg.dependencies["is-odd"] = "^99.0.0";
		fs.writeFileSync("package.json", JSON.stringify(pkg, null, 2));
	'
	run aube ci
	assert_failure
	assert_output --partial "lockfile is out of date"
}

@test "aube ci --ignore-scripts accepts the flag" {
	_setup_basic_fixture
	run aube ci --ignore-scripts
	assert_success
}

@test "aube clean-install is an alias for aube ci" {
	_setup_basic_fixture
	run aube clean-install
	assert_success
	assert_file_exists node_modules/is-odd/package.json
}

@test "aube ci removes a symlink node_modules without wiping its target" {
	# If node_modules is a symlink to an unrelated directory (rare but
	# legal), ci must unlink the symlink itself and NOT recursively delete
	# the target directory. remove_existing() in commands/mod.rs handles
	# this via a symlink check; this test guards against regressions where
	# a naive remove_dir_all would follow the symlink and wipe the target.
	_setup_basic_fixture
	mkdir -p "$TEST_TEMP_DIR/elsewhere"
	touch "$TEST_TEMP_DIR/elsewhere/must-survive.txt"
	ln -s "$TEST_TEMP_DIR/elsewhere" node_modules
	run aube ci
	assert_success
	# Target directory and its contents must still exist.
	assert_file_exists "$TEST_TEMP_DIR/elsewhere/must-survive.txt"
	# Fresh node_modules should be a real directory now, not the symlink.
	run test -L node_modules
	assert_failure
	assert_dir_exists node_modules
	assert_file_exists node_modules/is-odd/package.json
}
