#!/usr/bin/env bash
# One-time, maintainer-run: create the standalone-runner packages on npm and
# configure their trusted publishers, BEFORE the first release that ships them.
#
# Why this exists: npm's OIDC trusted publishing cannot perform a package's
# FIRST publish — the package must exist before a trusted publisher can be
# configured (npm/cli#8544) — so without this step, release.yml's publish jobs
# fail on all nine runner packages (their preflight step checks exactly this).
# This script publishes a 0.0.0 placeholder for each missing package (0.0.0
# sorts below every real release) and then points its trusted publisher at
# release.yml in nubjs/nub, matching how the existing @nubjs packages are
# configured. Requires `npm login` as a user with publish rights on the @nubjs
# org; safe to re-run (existing packages are skipped, an already-configured
# trust verifies clean).
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
root_name="$(node -p "require('$repo_root/npm/runner/package.json').name")"
packages=("$root_name")
for p in darwin-arm64 darwin-x64 linux-x64 linux-x64-musl linux-arm64 linux-arm64-musl win32-x64 win32-arm64; do
  packages+=("@nubjs/runner-$p")
done

# `npm trust` shipped in npm 11.10; refuse older npm up front rather than
# discovering it after the irreversible placeholder publishes.
node -e '
  const [a, b] = process.argv[1].split(".").map(Number);
  process.exit(a > 11 || (a === 11 && b >= 10) ? 0 : 1);
' "$(npm --version)" || { echo "npm >= 11.10 required (npm trust); found $(npm --version)"; exit 1; }
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
  # The command itself may exit non-zero on an already-configured publisher, so
  # the VERIFICATION is `npm trust list`: the configuration either names this
  # repo afterwards or the setup did not happen (wrong auth, 2FA, npm too old),
  # and a quiet miss here is exactly what strands the release publish later.
  npm trust github "$name" --file release.yml --repo nubjs/nub --allow-publish -y || true
  npm trust list "$name" 2>/dev/null | grep -q "nubjs/nub" || {
    echo "trust is NOT configured for $name — inspect with: npm trust list \"$name\""
    exit 1
  }
done

echo "done: all ${#packages[@]} packages exist and trust nubjs/nub release.yml"
