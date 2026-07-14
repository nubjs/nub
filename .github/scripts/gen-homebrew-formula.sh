#!/usr/bin/env bash
set -euo pipefail

# Regenerate the Homebrew formula for nubjs/homebrew-tap from a published release.
#
# Fills VERSION + the four GNU/macOS sha256s into Formula/nub.rb. The sha256s are
# READ from the release's own `.sha256` sidecar assets (the same bytes the release
# job uploaded) rather than recomputed, so this can never disagree with what was
# shipped. Homebrew targets macOS (arm/intel) + Linux GLIBC (arm/intel) only — the
# musl tarballs and the win32 .zip are not Homebrew-installable and are omitted.
#
# Usage: gen-homebrew-formula.sh <version> <output-path>
#   <version>     release version without the leading v (e.g. 0.1.14)
#   <output-path> where to write nub.rb
# Requires: gh (authed to read nubjs/nub releases).

VERSION="${1:?usage: gen-homebrew-formula.sh <version> <output-path>}"
OUT="${2:?usage: gen-homebrew-formula.sh <version> <output-path>}"
TAG="v${VERSION}"
REPO="nubjs/nub"

# Read a sha256 out of a release `.sha256` sidecar asset. The sidecar is
# `sha256sum` format (`<hex>  <name>`); take the first field. Fail loud if the
# asset is absent or the value isn't a 64-char hex digest — a bad formula must
# not be committed to the tap.
sidecar_sha256() {
  local target="$1" tmp sha
  tmp="$(mktemp)"
  if ! gh release download "$TAG" --repo "$REPO" \
        --pattern "nub-${target}.tar.gz.sha256" --output "$tmp" --clobber; then
    echo "::error::could not download nub-${target}.tar.gz.sha256 from $REPO@$TAG" >&2
    rm -f "$tmp"
    exit 1
  fi
  sha="$(awk '{print $1}' "$tmp")"
  rm -f "$tmp"
  if [[ ! "$sha" =~ ^[0-9a-f]{64}$ ]]; then
    echo "::error::nub-${target}.tar.gz.sha256 did not contain a valid sha256 (got: '$sha')" >&2
    exit 1
  fi
  printf '%s' "$sha"
}

SHA_DARWIN_ARM="$(sidecar_sha256 darwin-arm64)"
SHA_DARWIN_X64="$(sidecar_sha256 darwin-x64)"
SHA_LINUX_ARM="$(sidecar_sha256 linux-arm64)"
SHA_LINUX_X64="$(sidecar_sha256 linux-x64)"

BASE="https://github.com/${REPO}/releases/download/${TAG}"

cat > "$OUT" <<EOF
class Nub < Formula
  desc "Fast TypeScript runtime and package manager that augments Node"
  homepage "https://github.com/nubjs/nub"
  version "${VERSION}"
  license all_of: ["MIT", "LGPL-2.0-or-later", "BSD-3-Clause"]

  on_macos do
    on_arm do
      url "${BASE}/nub-darwin-arm64.tar.gz"
      sha256 "${SHA_DARWIN_ARM}"
    end
    on_intel do
      url "${BASE}/nub-darwin-x64.tar.gz"
      sha256 "${SHA_DARWIN_X64}"
    end
  end

  on_linux do
    on_arm do
      url "${BASE}/nub-linux-arm64.tar.gz"
      sha256 "${SHA_LINUX_ARM}"
    end
    on_intel do
      url "${BASE}/nub-linux-x64.tar.gz"
      sha256 "${SHA_LINUX_X64}"
    end
  end

  def install
    # nub is a single self-contained binary: it embeds its runtime (preload +
    # vendored polyfills + native addon) and JIT-extracts it to ~/.cache/nub on
    # first run, so there is no runtime sidecar to keep beside the binary. The archive ships
    # bin/ (nub + nubx, both real copies; nub picks its verb from the argv[0]
    # basename) plus a compatibility runtime/ layout for self-upgrade. Two top-level entries mean
    # Homebrew does NOT flatten a lone directory, so reference the binaries by their
    # bin/ path explicitly — install them straight onto PATH, no libexec, no symlink
    # dance. Linux also installs the adjacent Bubblewrap resource.
    bin.install "bin/nub", "bin/nubx"
    if OS.linux?
      (bin/"nub-resources").install Dir["bin/nub-resources/*"]
    end
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/nub --version")
    # Do NOT run a transpile here: \`brew test\` runs on a clean machine with no Node
    # on PATH, and nub augments the user's Node rather than bundling one.
  end
end
EOF

echo "✓ wrote $OUT for nub ${VERSION}"
