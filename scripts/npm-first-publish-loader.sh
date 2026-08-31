#!/usr/bin/env bash
# One-time, maintainer-run: create the standalone-loader packages on npm and
# configure their trusted publishers, BEFORE the first release that ships them.
#
# Why this exists: npm's OIDC trusted publishing cannot perform a package's
# FIRST publish — the package must exist before a trusted publisher can be
# configured (npm/cli#8544) — so without this step, release.yml's publish jobs
# fail on all nine loader packages. This script publishes a 0.0.0 placeholder
# for each missing package (0.0.0 sorts below every real release) and then
# points its trusted publisher at release.yml in nubjs/nub, matching how the
# existing @nubjs packages are configured. Requires `npm login` as a user with
# publish rights on the @nubjs org; safe to re-run (existing packages are
# skipped, re-trusting is reported but harmless).
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root_name="$(node -p "require('$repo_root/npm/loader/package.json').name")"
packages=("$root_name")
for p in darwin-arm64 darwin-x64 linux-x64 linux-x64-musl linux-arm64 linux-arm64-musl win32-x64 win32-arm64; do
  packages+=("@nubjs/loader-$p")
done

npm whoami >/dev/null || { echo "not logged in — run npm login first"; exit 1; }

for name in "${packages[@]}"; do
  if [ -n "$(npm view "$name" version 2>/dev/null)" ]; then
    echo "✓ $name exists — skipping publish"
  else
    dir="$(mktemp -d)"
    node -e "
      const fs = require('fs');
      fs.writeFileSync('$dir/package.json', JSON.stringify({
        name: '$name',
        version: '0.0.0',
        description: 'Placeholder — the Nub loader ships here with the next Nub release.',
        license: 'MIT',
        repository: 'https://github.com/nubjs/nub',
      }, null, 2) + '\n');
      fs.writeFileSync('$dir/README.md', 'Placeholder — the Nub loader ships here with the next Nub release. See https://github.com/nubjs/nub.\n');
    "
    echo "→ publishing $name@0.0.0 (placeholder)"
    (cd "$dir" && npm publish --access public) || { echo "publish failed for $name"; exit 1; }
  fi
  echo "→ trusting nubjs/nub release.yml for $name"
  npm trust github "$name" --file release.yml --repo nubjs/nub --allow-publish -y || echo "  (trust step reported an error for $name — check npm trust list \"$name\")"
done

echo "done. Verify with: npm trust list <package>"
