#!/usr/bin/env bash
# Start a local Verdaccio registry for nub integration tests.
# Usage: source tests/registry/start.bash
#
# Sets NUB_TEST_REGISTRY and VERDACCIO_PID in the environment on success.
# Call stop_registry to shut it down.
#
# The committed storage dir holds pre-seeded tarballs and packuments so
# tests don't need to reach npmjs.org. Every package in the storage was
# checked against the Socket.dev malware API before seeding (see
# check-safety.mts).
#
# Failure model: this file is `source`d, so a failure cannot exit the caller's
# shell. Instead it signals failure by NOT exporting NUB_TEST_REGISTRY and
# leaving VERDACCIO_PID unset — the Rust test guards read the var, so a dead
# registry self-skips the suite instead of flipping every guard to "run against
# a dead port".

REGISTRY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERDACCIO_PORT="${VERDACCIO_PORT:-4874}"
STORAGE_DIR="$REGISTRY_DIR/storage"
CONFIG_FILE="$REGISTRY_DIR/config.yaml"
REGISTRY_PROBE_PKG="is-positive"

# Clear any stale guard var from a prior successful source so a failure in
# THIS run self-skips the suite instead of inheriting a dead-registry URL.
unset NUB_TEST_REGISTRY

# Whether whatever is listening on $1 is OUR registry, rather than merely
# something that answers HTTP. An unrelated dev server on the port makes the
# health probe green while every fixture lookup 404s, so probe a package the
# committed storage is known to hold and require a real packument back.
registry_is_ours() {
	local port=$1 body
	body="$(curl -s --max-time 2 "http://localhost:${port}/${REGISTRY_PROBE_PKG}" 2>/dev/null)" || return 1
	case "$body" in
	*'"versions"'*) return 0 ;;
	*) return 1 ;;
	esac
}

stop_registry() {
	if [ -n "${VERDACCIO_PID:-}" ]; then
		kill "${VERDACCIO_PID}" 2>/dev/null || true
		unset VERDACCIO_PID
		echo "Verdaccio stopped"
	fi
}

# Reuse a registry that is genuinely ours (a previous suite left it up).
if registry_is_ours "$VERDACCIO_PORT"; then
	echo "Verdaccio already running on port $VERDACCIO_PORT"
	export NUB_TEST_REGISTRY="http://localhost:${VERDACCIO_PORT}"
	return 0 2>/dev/null || true
fi

# Something else owns the port. Step to a free one rather than silently using a
# foreign server. Callers read the port back out of NUB_TEST_REGISTRY, so moving
# is transparent.
if curl -s --max-time 2 "http://localhost:${VERDACCIO_PORT}/" >/dev/null 2>&1; then
	occupied=$VERDACCIO_PORT
	for probe in $(seq $((VERDACCIO_PORT + 1)) $((VERDACCIO_PORT + 20))); do
		if ! curl -s --max-time 1 "http://localhost:${probe}/" >/dev/null 2>&1; then
			VERDACCIO_PORT=$probe
			break
		fi
	done
	if [ "$VERDACCIO_PORT" = "$occupied" ]; then
		echo "ERROR: port $occupied is taken by a non-nub server and no free port found in the next 20" >&2
		return 1 2>/dev/null || exit 1
	fi
	echo "port $occupied is taken by a non-nub server; using $VERDACCIO_PORT instead" >&2
fi

# Install verdaccio if not already available (pinned to v6, which config.yaml's
# schema targets). tests/registry/package.json also pins verdaccio@6 for the CI
# path; this covers contributors who source this script without `npm ci` here.
if ! command -v verdaccio >/dev/null 2>&1; then
	echo "Installing verdaccio@6..." >&2
	npm install --global verdaccio@6 >&2 2>&1 | tail -1 >&2
	if ! command -v verdaccio >/dev/null 2>&1; then
		echo 'ERROR: verdaccio is not on PATH and `npm install --global verdaccio@6` failed' >&2
		return 1 2>/dev/null || exit 1
	fi
fi

# Absolute config path: verdaccio resolves a relative one against its own cwd,
# so a caller that starts the registry from anywhere but this directory would
# otherwise die with "cannot open config file".
verdaccio --config "$CONFIG_FILE" --listen "0.0.0.0:${VERDACCIO_PORT}" \
	> /tmp/nub-verdaccio.log 2>&1 &
VERDACCIO_PID=$!

# Wait for it to SERVE, not merely to bind — and fail closed if it never does.
# A readiness loop that falls through after N iterations (without checking the
# last probe) reports success for a registry that never came up, which is the
# exact failure this block exists to make loud.
retries=100
while ! registry_is_ours "$VERDACCIO_PORT"; do
	retries=$((retries - 1))
	if [ "$retries" -le 0 ]; then
		echo "ERROR: Verdaccio on port $VERDACCIO_PORT never served ${REGISTRY_PROBE_PKG} from $STORAGE_DIR (see /tmp/nub-verdaccio.log)" >&2
		kill "$VERDACCIO_PID" 2>/dev/null || true
		unset VERDACCIO_PID
		return 1 2>/dev/null || exit 1
	fi
	sleep 0.1
done

# Only export the guard var once the registry is confirmed serving. This is the
# one signal the Rust test guards read; setting it unconditionally would flip
# them from "skip" to "run against a dead port" on a failed start.
export NUB_TEST_REGISTRY="http://localhost:${VERDACCIO_PORT}"
export VERDACCIO_PID
echo "Verdaccio started on ${NUB_TEST_REGISTRY} (PID ${VERDACCIO_PID})"
