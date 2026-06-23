#!/usr/bin/env bats

assert_file_contains() {
	local file="$1"
	local needle="$2"
	run cat "$file"
	assert_success
	assert_output --partial "$needle"
}

# Start the mock npm web-login server (test/fixtures/web-login-server.mjs).
# Sets $MOCK_WEB_LOGIN_PORT and $MOCK_WEB_LOGIN_PID so the caller can hit
# it with `aube login --auth-type=web --registry=http://127.0.0.1:$PORT/`
# and then tear it down in teardown.
_start_mock_web_login() {
	MOCK_WEB_LOGIN_PORT_FILE="$BATS_TEST_TMPDIR/mock-web-login.port"
	rm -f "$MOCK_WEB_LOGIN_PORT_FILE"
	node "$PROJECT_ROOT/test/fixtures/web-login-server.mjs" \
		"$MOCK_WEB_LOGIN_PORT_FILE" 3>&- &
	MOCK_WEB_LOGIN_PID=$!

	local tries=40
	while [ "$tries" -gt 0 ]; do
		if [ -s "$MOCK_WEB_LOGIN_PORT_FILE" ]; then
			MOCK_WEB_LOGIN_PORT="$(cat "$MOCK_WEB_LOGIN_PORT_FILE")"
			return 0
		fi
		sleep 0.05
		tries=$((tries - 1))
	done

	echo "mock web-login server failed to start" >&2
	return 1
}

_stop_mock_web_login() {
	if [ -n "${MOCK_WEB_LOGIN_PID:-}" ]; then
		kill "$MOCK_WEB_LOGIN_PID" 2>/dev/null || true
		wait "$MOCK_WEB_LOGIN_PID" 2>/dev/null || true
		unset MOCK_WEB_LOGIN_PID MOCK_WEB_LOGIN_PORT MOCK_WEB_LOGIN_PORT_FILE
	fi
}

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_stop_mock_web_login
	_common_teardown
}

@test "aube login writes _authToken from env var" {
	AUBE_AUTH_TOKEN=tok123 run aube login --registry=https://r.example.com/
	assert_success
	assert_output --partial "Logged in to https://r.example.com/"
	assert_file_contains "$HOME/.npmrc" "//r.example.com/:_authToken=tok123"
}

@test "aube login writes scope mapping alongside token" {
	AUBE_AUTH_TOKEN=scoped run aube login \
		--registry=https://myorg.example.com/ \
		--scope=@myorg
	assert_success
	run cat "$HOME/.npmrc"
	assert_success
	assert_output --partial "//myorg.example.com/:@myorg:_authToken=scoped"
	assert_file_contains "$HOME/.npmrc" "@myorg:registry=https://myorg.example.com/"
}

@test "aube login normalizes scope casing" {
	AUBE_AUTH_TOKEN=scoped run aube login \
		--registry=https://myorg.example.com/ \
		--scope=@MyOrg
	assert_success
	run cat "$HOME/.npmrc"
	assert_success
	assert_output --partial "//myorg.example.com/:@myorg:_authToken=scoped"
	assert_output --partial "@myorg:registry=https://myorg.example.com/"
	refute_output --partial "@MyOrg"
}

@test "aube login replaces an existing token" {
	printf '%s\n' \
		'registry=https://r.example.com/' \
		'//r.example.com/:_authToken=old' >"$HOME/.npmrc"

	AUBE_AUTH_TOKEN=new run aube login --registry=https://r.example.com/
	assert_success

	run cat "$HOME/.npmrc"
	assert_success
	assert_output --partial "//r.example.com/:_authToken=new"
	refute_output --partial "=old"
}

@test "aube login reads token from piped stdin" {
	run bash -c 'printf "piped-token\n" | aube login --registry=https://r.example.com/'
	assert_success
	assert_file_contains "$HOME/.npmrc" "//r.example.com/:_authToken=piped-token"
}

@test "aube login --auth-type=web drives the OAuth flow end-to-end" {
	_start_mock_web_login
	AUBE_NO_BROWSER=1 run aube login \
		--auth-type=web \
		--registry="http://127.0.0.1:$MOCK_WEB_LOGIN_PORT/"
	assert_success
	assert_output --partial "Open this URL in your browser"
	assert_output --partial "/login-page"
	assert_output --partial "Waiting for authentication"
	assert_output --partial "Logged in to http://127.0.0.1:$MOCK_WEB_LOGIN_PORT/"
	assert_file_contains "$HOME/.npmrc" \
		"//127.0.0.1:$MOCK_WEB_LOGIN_PORT/:_authToken=mock-web-token"
}

@test "aube login --auth-type=web --scope writes the scope mapping" {
	_start_mock_web_login
	AUBE_NO_BROWSER=1 run aube login \
		--auth-type=web \
		--registry="http://127.0.0.1:$MOCK_WEB_LOGIN_PORT/" \
		--scope=@myorg
	assert_success
	run cat "$HOME/.npmrc"
	assert_success
	assert_output --partial \
		"//127.0.0.1:$MOCK_WEB_LOGIN_PORT/:@myorg:_authToken=mock-web-token"
	assert_file_contains "$HOME/.npmrc" \
		"@myorg:registry=http://127.0.0.1:$MOCK_WEB_LOGIN_PORT/"
}

@test "aube login rejects an unknown --auth-type" {
	AUBE_AUTH_TOKEN=x run aube login \
		--registry=https://r.example.com/ \
		--auth-type=sso
	assert_failure
	assert_output --partial "--auth-type=sso is not supported"
}

@test "aube login rejects a scope that is missing @" {
	AUBE_AUTH_TOKEN=x run aube login --registry=https://r.example.com/ --scope=myorg
	assert_failure
	assert_output --partial "--scope must start with"
}

@test "aube logout removes the token and leaves other entries" {
	printf '%s\n' \
		'# keep me' \
		'registry=https://r.example.com/' \
		'//r.example.com/:_authToken=tok' >"$HOME/.npmrc"

	run aube logout --registry=https://r.example.com/
	assert_success
	assert_output --partial "Logged out of https://r.example.com/"

	run cat "$HOME/.npmrc"
	assert_success
	assert_output --partial "# keep me"
	assert_output --partial "registry=https://r.example.com/"
	refute_output --partial "_authToken"
}

@test "aube logout --scope strips the scope mapping too" {
	printf '%s\n' \
		'@myorg:registry=https://myorg.example.com/' \
		'//myorg.example.com/:@myorg:_authToken=tok' >"$HOME/.npmrc"

	run aube logout --scope=@myorg --registry=https://myorg.example.com/
	assert_success

	run cat "$HOME/.npmrc"
	assert_success
	refute_output --partial "_authToken"
	refute_output --partial "@myorg:registry"
}

@test "aube logout --scope matches scope casing insensitively" {
	printf '%s\n' \
		'@MyOrg:registry=https://myorg.example.com/' \
		'//myorg.example.com/:@MyOrg:_authToken=tok' >"$HOME/.npmrc"

	run aube logout --scope=@myorg --registry=https://myorg.example.com/
	assert_success

	run cat "$HOME/.npmrc"
	assert_success
	refute_output --partial "_authToken"
	refute_output --partial "@MyOrg:registry"
}

@test "aube logout without scope removes scoped tokens for the registry" {
	printf '%s\n' \
		'@myorg:registry=https://myorg.example.com/' \
		'//myorg.example.com/:@myorg:_authToken=scoped' \
		'//myorg.example.com/:_authToken=unscoped' >"$HOME/.npmrc"

	run aube logout --registry=https://myorg.example.com/
	assert_success
	assert_output --partial "Logged out of https://myorg.example.com/"

	run cat "$HOME/.npmrc"
	assert_success
	refute_output --partial "_authToken"
	assert_output --partial "@myorg:registry=https://myorg.example.com/"
}

@test "aube logout is a no-op when no credentials exist" {
	run aube logout --registry=https://r.example.com/
	assert_success
	assert_output --partial "nothing to do"
}
