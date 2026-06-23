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

start_registry() {
	# Check if already running
	if curl -s "http://localhost:${VERDACCIO_PORT}/" >/dev/null 2>&1; then
		echo "Verdaccio already running on port $VERDACCIO_PORT" >&2
		return 0
	fi

	# Install verdaccio if not already available (pinned major version)
	if ! command -v verdaccio >/dev/null 2>&1; then
		echo "Installing verdaccio..." >&2
		npm install --global verdaccio@6 2>&1 | tail -1 >&2
	fi

	verdaccio --config "$CONFIG_FILE" --listen "$VERDACCIO_PORT" &
	VERDACCIO_PID=$!
	export VERDACCIO_PID

	# Wait for it to come up (up to 30 seconds)
	local retries=60
	while ! curl -s "http://localhost:${VERDACCIO_PORT}/" >/dev/null 2>&1; do
		retries=$((retries - 1))
		if [ "$retries" -le 0 ]; then
			echo "ERROR: Verdaccio failed to start on port $VERDACCIO_PORT" >&2
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
