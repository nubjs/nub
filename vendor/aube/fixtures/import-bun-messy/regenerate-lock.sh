#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

registry="${AUBE_TEST_REGISTRY:-http://localhost:4873}"

if ! curl -s "$registry/" >/dev/null 2>&1; then
	echo "Verdaccio registry is not reachable at $registry" >&2
	echo "Start it first with: source test/registry/start.bash && start_registry" >&2
	exit 1
fi

cleanup() {
	find . -name node_modules -type d -prune -exec rm -rf {} +
	rm -f .npmrc aube-builds-marker*.txt
}
trap cleanup EXIT

printf 'registry=%s\n' "$registry" >.npmrc

rm -rf node_modules
bun install
