#!/usr/bin/env bash
# Fail if reqwest's hickory-dns feature is enabled anywhere in the nub-cli
# graph, on any target. Through feature unification it switches every reqwest
# client in the binary to Hickory, which ignores VPN-scoped and split DNS
# (#942); the registry clients resolve through the OS with an in-process cache
# instead (vendor/aube/crates/aube-util/src/http/dns.rs).
# @lat: [[design/architecture#Architecture#HTTP DNS resolution]]
set -euo pipefail
cd "$(dirname "$0")/.."

for features in default all; do
  args=(tree --locked -p nub-cli --target all
    --edges normal --invert reqwest --depth 0 --format '{p} {f}')
  if [[ "$features" == all ]]; then args+=(--all-features); fi
  reqwest=$(cargo "${args[@]}")
  if [[ -z "$reqwest" ]]; then
    echo "FAIL: reqwest not found in the nub-cli graph ($features features)" >&2
    exit 1
  fi
  if grep -Eq '[ ,]hickory-dns(,|$)' <<< "$reqwest"; then
    echo "FAIL: reqwest/hickory-dns is enabled ($features features):" >&2
    echo "$reqwest" >&2
    exit 1
  fi
  echo "PASS: reqwest/hickory-dns is off ($features features)"
done
