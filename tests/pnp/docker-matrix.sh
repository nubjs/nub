#!/bin/bash
# Run the Yarn PnP matrix on Linux across several Node versions, each in its own
# container (see Dockerfile.pnp). Builds a Linux nub once (cached in the builder
# stage) and layers it onto each node:<ver> base.
#
# Usage: tests/pnp/docker-matrix.sh [node-version ...]
# Defaults to the compat floor + current LTS lines. Requires Docker.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
VERSIONS=("$@")
[ ${#VERSIONS[@]} -gt 0 ] || VERSIONS=(18.19 20 22 24)

if ! docker version >/dev/null 2>&1; then echo "error: Docker not available" >&2; exit 1; fi

fails=0
for v in "${VERSIONS[@]}"; do
  echo "── Node $v ─────────────────────────────────────────────"
  if docker build -q -f "$SCRIPT_DIR/Dockerfile.pnp" --build-arg "NODE_VERSION=$v" -t "nub-pnp:$v" "$REPO_DIR" >/dev/null; then
    docker run --rm "nub-pnp:$v" || fails=$((fails+1))
  else
    echo "Node $v: image build FAILED" >&2; fails=$((fails+1))
  fi
done

echo
[ "$fails" -eq 0 ] && echo "Docker PnP matrix: all versions passed." || echo "Docker PnP matrix: $fails version(s) FAILED."
exit $fails
