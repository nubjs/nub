#!/usr/bin/env bash
# Start a local Verdaccio registry for integration tests.
# Usage: source test/registry/start.bash
#
# Sets VERDACCIO_PID and AUBE_TEST_REGISTRY in the environment.
# Call stop_registry to shut it down.

REGISTRY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERDACCIO_PORT="${VERDACCIO_PORT:-4873}"

export AUBE_TEST_REGISTRY="http://localhost:${VERDACCIO_PORT}"

# The committed storage dir holds pre-seeded tarballs and packuments so
# tests don't need to reach npmjs.org. `storage: ./storage` in config.yaml
# resolves relative to the config file's directory, so we use the source
# config directly without a runtime rewrite.
STORAGE_DIR="$REGISTRY_DIR/storage"
CONFIG_FILE="$REGISTRY_DIR/config.yaml"
mkdir -p "$STORAGE_DIR"

# Whether whatever is listening on $1 is OUR registry, rather than merely
# something that answers HTTP.
#
# "Does the port respond?" is not the same question, and getting it wrong is
# silent: an unrelated dev server on 4873 makes `start_registry` report success
# while every fixture lookup 404s, so the whole suite fails with "package not
# found" and no hint that it is talking to a stranger. Probe a package the
# committed storage is known to hold and require a real packument back.
registry_is_ours() {
	local port=$1 body
	body="$(curl -s --max-time 2 "http://localhost:${port}/${REGISTRY_PROBE_PKG}" 2>/dev/null)" || return 1
	case "$body" in
	*'"versions"'*) return 0 ;;
	*) return 1 ;;
	esac
}
REGISTRY_PROBE_PKG="abbrev"

start_registry() {
	# Reuse a registry that is genuinely ours (a previous suite left it up).
	if registry_is_ours "$VERDACCIO_PORT"; then
		echo "Verdaccio already running on port $VERDACCIO_PORT" >&2
		return 0
	fi

	# Something else owns the port. Step to a free one rather than failing or,
	# worse, silently using it. Every caller reads the port back out of
	# AUBE_TEST_REGISTRY, so moving is transparent.
	if curl -s --max-time 2 "http://localhost:${VERDACCIO_PORT}/" >/dev/null 2>&1; then
		local occupied=$VERDACCIO_PORT probe
		for probe in $(seq $((VERDACCIO_PORT + 1)) $((VERDACCIO_PORT + 20))); do
			if ! curl -s --max-time 1 "http://localhost:${probe}/" >/dev/null 2>&1; then
				VERDACCIO_PORT=$probe
				break
			fi
		done
		if [ "$VERDACCIO_PORT" = "$occupied" ]; then
			echo "ERROR: port $occupied is taken by a non-aube server and no free port found in the next 20" >&2
			return 1
		fi
		echo "port $occupied is taken by a non-aube server; using $VERDACCIO_PORT instead" >&2
		export AUBE_TEST_REGISTRY="http://localhost:${VERDACCIO_PORT}"
	fi

	# Install verdaccio if not already available (pinned major version)
	if ! command -v verdaccio >/dev/null 2>&1; then
		echo "Installing verdaccio..." >&2
		npm install --global verdaccio@6 2>&1 | tail -1 >&2
	fi

	# Absolute config path: verdaccio resolves a relative one against its own
	# cwd, so a caller that starts the registry from anywhere but this
	# directory otherwise dies with "cannot open config file".
	verdaccio --config "$CONFIG_FILE" --listen "$VERDACCIO_PORT" &
	VERDACCIO_PID=$!
	export VERDACCIO_PID

	# Wait for it to SERVE, not merely to bind. A verdaccio that came up
	# against the wrong storage answers on / immediately and 404s every
	# package, which is the failure this whole function exists to make loud.
	local retries=60
	while ! registry_is_ours "$VERDACCIO_PORT"; do
		retries=$((retries - 1))
		if [ "$retries" -le 0 ]; then
			echo "ERROR: Verdaccio on port $VERDACCIO_PORT never served ${REGISTRY_PROBE_PKG} from $STORAGE_DIR" >&2
			kill "$VERDACCIO_PID" 2>/dev/null || true
			return 1
		fi
		sleep 0.5
	done

	echo "Verdaccio started on $AUBE_TEST_REGISTRY (PID $VERDACCIO_PID)" >&2
}

stop_registry() {
	if [ -n "${VERDACCIO_PID:-}" ]; then
		kill "$VERDACCIO_PID" 2>/dev/null || true
		wait "$VERDACCIO_PID" 2>/dev/null || true
		unset VERDACCIO_PID
		echo "Verdaccio stopped" >&2
	fi
}
