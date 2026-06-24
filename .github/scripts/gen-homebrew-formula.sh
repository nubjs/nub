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
  license "MIT"

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
    # The release archive is a tree (bin/nub, bin/nubx, runtime/), not a bare
    # binary: nub loads runtime/ (preload + vendored polyfills + native addon)
    # relative to the real binary, so the whole tree must stay together. Install
    # it into libexec and symlink the executables onto PATH. Plain symlinks (no
    # wrapper script) so each call execs the native binary with zero overhead;
    # nub canonicalizes current_exe() to find runtime/ beside the real binary.
    libexec.install Dir["*"]
    bin.install_symlink libexec/"bin/nub"
    bin.install_symlink libexec/"bin/nubx"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/nub --version")

    # Prove runtime/ was installed correctly: a bare-binary install passes
    # --version but cannot transpile TS (it needs runtime/preload).
    (testpath/"hello.ts").write("const x: string = \"hi nub\";\nconsole.log(x);\n")
    assert_equal "hi nub", shell_output("#{bin}/nub #{testpath}/hello.ts").strip
  end
end
EOF

echo "✓ wrote $OUT for nub ${VERSION}"
