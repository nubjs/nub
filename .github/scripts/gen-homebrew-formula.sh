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
    # nub is a single self-contained binary: it embeds its runtime (preload +
    # vendored polyfills + native addon) and JIT-extracts it to ~/.cache/nub on
    # first run, so there is no sidecar to keep beside the binary. The archive ships
    # bin/ (one real binary, bin/nub) PLUS a vestigial empty runtime/ that exists
    # only to satisfy the sidecar-era \`nub upgrade\` (see release.yml). Two top-level
    # entries means Homebrew does NOT flatten a lone directory, so reference the
    # binary by its bin/ path explicitly — install it straight onto PATH, no libexec,
    # and ignore runtime/.
    bin.install "bin/nub"
    # \`nubx\` is the same binary under a second name: nub reads its verb from the
    # argv[0] basename (Argv0::detect in crates/nub-cli/src/cli.rs). Only one copy
    # ships, so the alias is created here — install.sh, install.ps1 and flake.nix
    # each do the same for their own channel.
    bin.install_symlink bin/"nub" => "nubx"
    # The nub compile launcher template resolves as a SIBLING of the running nub
    # (compile::launcher::locate), so it has to land wherever the binary did —
    # libexec would put it out of reach. Accepted cost: brew links the keg's bin
    # into the prefix, so the template becomes a (harmless, namespaced) entry on
    # PATH. Globbed, so this still installs from a pre-template archive: this
    # branch stages it at bin/nub-launcher-<platform> (release.yml), main does not.
    bin.install Dir["bin/nub-launcher-*"]
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/nub --version")
    # The alias must EXIST and DISPATCH. \`nubx --help\` prints the exec grammar,
    # which plain \`nub\` never does — so this fails both if bin/nubx is missing and
    # if it somehow resolves back to the top-level CLI.
    assert_match "Usage: nub nubx", shell_output("#{bin}/nubx --help")
    # Do NOT run a transpile here: \`brew test\` runs on a clean machine with no Node
    # on PATH, and nub augments the user's Node rather than bundling one.
  end
end
EOF

echo "✓ wrote $OUT for nub ${VERSION}"
