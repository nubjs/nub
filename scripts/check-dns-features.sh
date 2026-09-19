#!/usr/bin/env bash
# Guard the resolved reqwest features, including feature unification from
# auxiliary HTTP clients. No cross-compilation or live VPN is needed.
# @lat: [[design/architecture#Architecture#HTTP DNS resolution]]
set -euo pipefail
cd "$(dirname "$0")/.."

targets=(
  aarch64-apple-darwin
  x86_64-apple-darwin
  x86_64-unknown-linux-gnu
  x86_64-pc-windows-msvc
  aarch64-linux-android
)
# Android still compiles Hickory; the engine's client builders disable it
# at runtime to avoid JNI without a JVM. This guards feature selection only.
for target in "${targets[@]}"; do
  for features in default all; do
    args=(tree --locked -p nub-cli --target "$target"
      --edges normal --invert reqwest --depth 0 --format '{f}')
    if [[ "$features" == all ]]; then args+=(--all-features); fi
    reqwest_features=$(cargo "${args[@]}")
    if [[ -z "$reqwest_features" ]]; then
      echo "FAIL: no reqwest features found for $target ($features)" >&2
      exit 1
    fi
    if grep -Eq '(^|,)hickory-dns(,|$)' <<< "$reqwest_features"; then
      hickory=true
    else
      hickory=false
    fi
    expected=true
    if [[ "$target" == *-apple-darwin ]]; then expected=false; fi
    if [[ "$hickory" != "$expected" ]]; then
      echo "FAIL: $target ($features): reqwest/hickory-dns=$hickory, expected $expected" >&2
      exit 1
    fi
    echo "PASS: $target ($features): reqwest/hickory-dns=$hickory"
  done
done
