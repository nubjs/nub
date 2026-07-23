#!/usr/bin/env bats

setup() {
	load 'test_helper/common_setup'
	_common_setup
}

teardown() {
	_common_teardown
}

@test "aube exec --parallel fans out across workspace packages" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/a packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"0.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"0.0.0","dependencies":{"is-odd":"3.0.1"}}
EOF

	run aube install
	assert_success

	run aube -r exec --parallel --shell-mode node -- -e 'console.log(require("is-odd")(3) ? process.cwd().split("/").pop() : "no")'
	assert_success
	assert_output --partial "a"
	assert_output --partial "b"
}

@test "aube exec --parallel preflights missing binaries before spawning" {
	cat >pnpm-workspace.yaml <<'EOF'
packages:
  - packages/*
EOF
	cat >package.json <<'EOF'
{"name":"root","version":"0.0.0","private":true}
EOF
	mkdir -p packages/a/node_modules/.bin packages/b
	cat >packages/a/package.json <<'EOF'
{"name":"a","version":"0.0.0"}
EOF
	cat >packages/b/package.json <<'EOF'
{"name":"b","version":"0.0.0"}
EOF
	cat >packages/a/node_modules/.bin/sentinel <<'EOF'
#!/usr/bin/env bash
echo ran >../../sentinel-ran
EOF
	chmod +x packages/a/node_modules/.bin/sentinel

	run aube install
	assert_success
	mkdir -p packages/a/node_modules/.bin
	cat >packages/a/node_modules/.bin/sentinel <<'EOF'
#!/usr/bin/env bash
echo ran >../../sentinel-ran
EOF
	chmod +x packages/a/node_modules/.bin/sentinel

	run aube -r exec --parallel sentinel
	assert_failure
	assert_output --partial "binary not found in b: sentinel"
	assert_file_not_exist packages/sentinel-ran
}

# Regression: terminating the aube process must tear down the tool it
# launched instead of orphaning it under init. See discussion #1059.
#
# Parameterized over the launcher and signal so both teardown mechanisms
# are covered:
#   - `aube exec` on the standalone binary replaces its image with the tool
#     (execvp), so the tool inherits aube's pid and any signal — SIGTERM or
#     SIGKILL, on any Unix — reaches it directly.
#   - `aube run <local-bin>` spawns the tool as a supervised child, so
#     teardown goes through process_guard: SIGTERM is forwarded (any Unix),
#     SIGKILL relies on Linux PR_SET_PDEATHSIG.
_assert_not_orphaned() {
	local launcher="$1" sig="$2" dur="$3"
	cat >package.json <<'EOF'
{"name":"t","version":"0.0.0"}
EOF
	mkdir -p node_modules/.bin
	printf '#!/bin/sh\nexec sleep %s\n' "$dur" >node_modules/.bin/sleeper
	chmod +x node_modules/.bin/sleeper

	aube "$launcher" --no-install sleeper &
	local aube_pid=$!

	local child=""
	for _ in $(seq 1 50); do
		child="$(pgrep -f "sleep ${dur}\$" || true)"
		[ -n "$child" ] && break
		sleep 0.1
	done
	[ -n "$child" ] || {
		kill "$aube_pid" 2>/dev/null || true
		fail "launched tool never started"
	}

	kill "-${sig}" "$aube_pid" 2>/dev/null || true

	local gone=""
	for _ in $(seq 1 30); do
		pgrep -f "sleep ${dur}\$" >/dev/null || {
			gone=1
			break
		}
		sleep 0.1
	done
	pkill -f "sleep ${dur}\$" 2>/dev/null || true
	[ -n "$gone" ] || fail "tool orphaned after ${sig} to aube ${launcher}"
}

@test "aube exec is not orphaned on SIGTERM (image replacement)" {
	[ "$(uname -s)" != "Windows_NT" ] || skip "signals are POSIX"
	_assert_not_orphaned exec TERM 987651
}

# Image replacement makes the tool inherit aube's pid, so SIGKILL teardown
# works on every Unix here — no PDEATHSIG, no macOS gap for this path.
@test "aube exec is not orphaned on SIGKILL (image replacement)" {
	[ "$(uname -s)" != "Windows_NT" ] || skip "signals are POSIX"
	_assert_not_orphaned exec KILL 987652
}

# `aube run <local-bin>` keeps the supervised-child path, so these cover
# process_guard's signal forwarding and PDEATHSIG directly.
@test "aube run forwards SIGTERM to the supervised tool" {
	[ "$(uname -s)" != "Windows_NT" ] || skip "signals are POSIX"
	_assert_not_orphaned run TERM 987654
}

@test "aube run reaps the supervised tool on SIGKILL (Linux PDEATHSIG)" {
	[ "$(uname -s)" = "Linux" ] || skip "PR_SET_PDEATHSIG is Linux-only"
	_assert_not_orphaned run KILL 987655
}

# The standalone binary replaces its image with the tool (execvp) rather
# than spawning a child, so the tool inherits aube's pid and no separate
# aube process is left behind. This is what makes a SIGKILL of that pid
# hit the tool directly, closing the macOS gap for this path.
@test "aube exec replaces its image with the tool (no separate aube process)" {
	[ "$(uname -s)" != "Windows_NT" ] || skip "execvp is POSIX"
	dur=987653
	cat >package.json <<'EOF'
{"name":"t","version":"0.0.0"}
EOF
	mkdir -p node_modules/.bin
	printf '#!/bin/sh\nexec sleep %s\n' "$dur" >node_modules/.bin/sleeper
	chmod +x node_modules/.bin/sleeper

	aube exec --no-install sleeper &
	launched=$!

	# Tear down the tool no matter which assertion fires — bats exits the
	# function immediately on `fail`, so cleanup has to run before each one
	# (mirrors `_assert_child_torn_down_on`).
	cleanup() {
		kill -KILL "$launched" 2>/dev/null || true
		pkill -f "sleep ${dur}\$" 2>/dev/null || true
	}

	child=""
	for _ in $(seq 1 50); do
		child="$(pgrep -f "sleep ${dur}\$" || true)"
		[ -n "$child" ] && break
		sleep 0.1
	done
	[ -n "$child" ] || {
		cleanup
		fail "tool never started"
	}

	# The launched pid was aube; after execvp it is the tool itself, so the
	# sleep runs *as* that pid and its comm is no longer aube.
	[ "$child" = "$launched" ] || {
		cleanup
		fail "tool did not inherit aube's pid (image not replaced): launched=$launched tool=$child"
	}
	comm="$(ps -o comm= -p "$launched" | tr -d ' ')"
	[ "$comm" != "aube" ] || {
		cleanup
		fail "aube image was not replaced (comm still aube)"
	}

	cleanup
}
