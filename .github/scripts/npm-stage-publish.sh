#!/usr/bin/env bash
# Stage one package directory for publishing: `npm stage publish` through OIDC trusted publishing.
#
# A staged version is not installable until a maintainer approves it with 2FA (`pnpm stage
# approve`, or the Staged Packages tab on npmjs.com). CI can only stage, because the trusted
# publisher on every @nubjs package is stage-only: a stolen push credential that cuts a tag can
# fill the staged queue and nothing else — which is what the 2026-09-21 v0.9.4 incident needed.
#
# Idempotent on a re-run. A version already live is skipped by the `npm view` check; a version
# already sitting in the staged queue makes `npm stage publish` refuse (npm reserves the number
# while it is staged), and that refusal is a success here: the approval wait downstream still
# gates on the registry serving the version. The refusal's exact wording is matched loosely
# because the first stage-only release is where it gets observed; anything else stays fatal.
set -euo pipefail
dir="${1:?usage: npm-stage-publish.sh <package-dir>}"
version="${VERSION:?VERSION must name the version being released}"
name="$(node -p "require('$dir/package.json').name")"
if [ "$(npm view "$name@$version" version 2>/dev/null)" = "$version" ]; then
  echo "✓ $name@$version already published — skipping"
  exit 0
fi
echo "→ staging $name@$version"
out="$(mktemp)"
if (cd "$dir" && npm stage publish --access public) >"$out" 2>&1; then
  cat "$out"; rm -f "$out"; exit 0
fi
cat "$out"
if grep -qiE 'already (been )?staged|staged version|E409|EPUBLISHCONFLICT|previously published' "$out"; then
  echo "✓ $name@$version is already staged or published — the approval wait decides the rest"
  rm -f "$out"; exit 0
fi
rm -f "$out"
echo "::error::staging $name@$version failed"
exit 1
