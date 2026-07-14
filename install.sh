#!/usr/bin/env bash
set -euo pipefail

# Nub installer — downloads the latest release binary from GitHub.
# Usage: curl -fsSL https://raw.githubusercontent.com/nubjs/nub/main/install.sh | bash
#
# Customization (env vars):
#   NUB_INSTALL_DIR      install location, absolute path (default: ~/.nub)
#   NUB_NO_MODIFY_PATH   truthy (1/yes/true/on) to skip editing your shell profile

# Windows: delegate to PowerShell
if [[ ${OS:-} = Windows_NT ]]; then
    powershell -c "irm https://raw.githubusercontent.com/nubjs/nub/main/install.ps1 | iex"
    exit $?
fi

Color_Off=''
Red=''
Green=''
Dim=''
Bold=''

if [[ -t 1 ]]; then
    Color_Off='\033[0m'
    Red='\033[0;31m'
    Green='\033[0;32m'
    Dim='\033[0;2m'
    Bold='\033[1m'
fi

error() { echo -e "${Red}error${Color_Off}: $*" >&2; exit 1; }
info() { echo -e "${Dim}$*${Color_Off}"; }
success() { echo -e "${Green}$*${Color_Off}"; }

parse_sha256_sidecar() {
    local sidecar=$1
    local archive_name=$2
    local expected_size actual_size

    expected_size=$((67 + ${#archive_name}))
    actual_size=$(wc -c < "$sidecar" | tr -d '[:space:]') || return 1
    [[ "$actual_size" == "$expected_size" ]] || return 1

    LC_ALL=C awk -v name="$archive_name" '
        NR != 1 { exit 1 }
        {
            digest = substr($0, 1, 64)
            if (length(digest) != 64 || digest ~ /[^0-9A-Fa-f]/ ||
                substr($0, 65, 2) != "  " || substr($0, 67) != name) {
                exit 1
            }
            print tolower(digest)
        }
        END { if (NR != 1) exit 1 }
    ' "$sidecar"
}

sha256_file() {
    local file=$1
    local output digest

    if command -v sha256sum >/dev/null 2>&1 && output=$(sha256sum "$file" 2>/dev/null); then
        digest=$(printf '%s\n' "$output" | LC_ALL=C awk 'NR == 1 && length($1) == 64 && $1 !~ /[^0-9A-Fa-f]/ { print tolower($1); exit }')
        if [[ -n "$digest" ]]; then
            printf '%s\n' "$digest"
            return 0
        fi
    fi
    if command -v shasum >/dev/null 2>&1 && output=$(shasum -a 256 "$file" 2>/dev/null); then
        digest=$(printf '%s\n' "$output" | LC_ALL=C awk 'NR == 1 && length($1) == 64 && $1 !~ /[^0-9A-Fa-f]/ { print tolower($1); exit }')
        if [[ -n "$digest" ]]; then
            printf '%s\n' "$digest"
            return 0
        fi
    fi
    return 1
}

# --- Platform detection ---

platform=$(uname -ms)

case "$platform" in
    'Darwin arm64')   target=darwin-arm64 ;;
    'Darwin x86_64')  target=darwin-x64 ;;
    'Linux aarch64' | 'Linux arm64') target=linux-arm64 ;;
    'Linux x86_64')   target=linux-x64 ;;
    *)                error "Unsupported platform: $platform" ;;
esac

# Detect musl (Alpine)
if [[ "$target" == linux-* ]]; then
    if [ -f /etc/alpine-release ] || (ldd --version 2>&1 | grep -qi musl); then
        target="${target}-musl"
    fi
fi

# Detect Rosetta
if [[ "$target" == darwin-x64 ]]; then
    if [[ $(sysctl -n sysctl.proc_translated 2>/dev/null) == 1 ]]; then
        target=darwin-arm64
        info "Your shell is running in Rosetta 2. Installing native ARM64 binary."
    fi
fi

# --- Version ---

version=${1:-latest}
if [[ "$version" == latest ]]; then
    # Authenticate the GitHub API call when a token is available: CI runners share
    # an IP and hit the 60/hr unauthenticated rate limit (403). Real users without
    # GITHUB_TOKEN use the anonymous path unchanged.
    api_auth=()
    [[ -n "${GITHUB_TOKEN:-}" ]] && api_auth=(-H "Authorization: token ${GITHUB_TOKEN}")
    version=$(curl -fsSL ${api_auth[@]+"${api_auth[@]}"} "https://api.github.com/repos/nubjs/nub/releases/latest" | grep '"tag_name"' | sed -E 's/.*"v(.*)".*/\1/')
    if [[ -z "$version" ]]; then
        error "Failed to determine latest version"
    fi
fi

# --- Install ---

# The install location is overridable via NUB_INSTALL_DIR (default ~/.nub). A
# custom dir is normalized to an absolute path so the PATH line, the receipt, and
# the "is this the default location?" test below are all exact. The default keeps
# the literal "$HOME/.nub" spelling so the emitted PATH line stays $HOME-portable.
default_install_dir="$HOME/.nub"
if [[ -n "${NUB_INSTALL_DIR:-}" ]]; then
    install_dir="$NUB_INSTALL_DIR"
    mkdir -p "$install_dir" || error "Failed to create install directory: $install_dir"
    install_dir=$(cd "$install_dir" && pwd) || error "Invalid NUB_INSTALL_DIR: $NUB_INSTALL_DIR"
else
    install_dir="$default_install_dir"
fi
bin_dir="$install_dir/bin"
exe="$bin_dir/nub"

info "Installing nub v${version} for ${target}..."

mkdir -p "$bin_dir" || error "Failed to create install directory: $bin_dir"

# Download the per-platform archive and extract it into the install dir. nub is a
# single self-contained binary that embeds its runtime (preload + vendored
# node_modules + native addon) and JIT-extracts it to ~/.cache/nub on first run.
# The archive ships bin/ plus a vestigial empty runtime/ (kept only to satisfy the
# sidecar-era `nub upgrade`; the binary ignores ~/.nub/runtime — see release.yml).
# (Windows is handled by install.ps1 above, so $target is always darwin/linux.)
archive_name="nub-${target}.tar.gz"
url="https://github.com/nubjs/nub/releases/download/v${version}/${archive_name}"
checksum_url="${url}.sha256"

tmp_archive=$(mktemp) || error "Failed to create temp file"
tmp_checksum=$(mktemp) || { rm -f "$tmp_archive"; error "Failed to create temp file"; }
trap 'rm -f "$tmp_archive" "$tmp_checksum"' EXIT

curl --fail --location --progress-bar --output "$tmp_archive" "$url" ||
    error "Failed to download nub from: $url"
curl --fail --location --progress-bar --output "$tmp_checksum" "$checksum_url" ||
    error "Failed to download checksum from: $checksum_url"

# The sidecar detects corrupt, truncated, stale-cache, or mismatched assets. It
# is not an independent authenticity check because both files share an origin.
if ! expected_sha256=$(parse_sha256_sidecar "$tmp_checksum" "$archive_name"); then
    error "Malformed checksum from: $checksum_url"
fi
if ! actual_sha256=$(sha256_file "$tmp_archive"); then
    error "No usable SHA-256 tool found (install sha256sum or shasum)"
fi
if [[ "$actual_sha256" != "$expected_sha256" ]]; then
    error "Checksum mismatch for $url (expected $expected_sha256, got $actual_sha256). Refusing to install a corrupt or mismatched archive."
fi

# Replace any prior nub artifacts for a clean upgrade. In the default ~/.nub —
# which nub owns outright — drop the whole bin/ and a stale runtime/ from a
# pre-single-binary install. A user-supplied NUB_INSTALL_DIR may hold unrelated
# files, so there remove only the two executables we wrote. Then extract bin/.
if [[ "$install_dir" == "$default_install_dir" ]]; then
    rm -rf "${install_dir:?}/bin" "${install_dir:?}/runtime"
else
    rm -f "${bin_dir:?}/nub" "${bin_dir:?}/nubx"
fi
tar -xzf "$tmp_archive" -C "$install_dir" ||
    error "Failed to extract nub archive from: $url"

[[ -f "$exe" ]] || error "Archive did not contain bin/nub"
chmod +x "$exe" || error "Failed to set permissions on $exe"

# `nubx` is the same binary as `nub`, dispatched on argv[0] (cli.rs reads
# args_os()[0].file_stem(): "nubx" -> exec). The release archive ships only
# bin/nub, so create the nubx alias as a relative symlink alongside it. `-f`
# makes this idempotent across reinstall/upgrade and harmless if a future
# archive ever ships its own nubx. Relative target keeps it valid if ~/.nub moves.
ln -sf nub "$bin_dir/nubx" || error "Failed to create nubx symlink in $bin_dir"

# Install receipt: marks this dir as a nub self-managed install so `nub upgrade`
# recognizes it as in-place-upgradeable even when NUB_INSTALL_DIR relocated it out
# of the default ~/.nub (cli.rs detect_channel checks for this file). Survives an
# upgrade — the self-owned swap only touches bin/.
cat > "$install_dir/.nub-receipt" <<'RECEIPT' || error "Failed to write install receipt to $install_dir"
# This file marks a nub self-managed install so `nub upgrade` can update it in
# place. Created by the nub installer; safe to delete (deleting it disables
# in-place self-update for a non-default install location).
RECEIPT

success "Installed nub v${version} (with nubx) to $exe"

# --- PATH setup ---

tildify() {
    if [[ $1 == "$HOME"/* ]]; then
        echo "~${1#$HOME}"
    else
        echo "$1"
    fi
}

tilde_bin_dir=$(tildify "$bin_dir")

# PATH export lines reference $HOME (kept $HOME-relative when bin_dir is under home)
# so they stay portable across machines; an out-of-home custom dir uses its absolute
# path.
# Both lines quote the directory so a custom NUB_INSTALL_DIR containing spaces
# survives (fish word-splits an unquoted path). The default ~/.nub/bin has no
# spaces, so its emitted lines are unchanged in effect.
if [[ "$bin_dir" == "$HOME"/* ]]; then
    posix_path_line="export PATH=\"\$HOME/${bin_dir#"$HOME"/}:\$PATH\""
    fish_path_line="set -gx PATH \"\$HOME/${bin_dir#"$HOME"/}\" \$PATH"
else
    posix_path_line="export PATH=\"$bin_dir:\$PATH\""
    fish_path_line="set -gx PATH \"$bin_dir\" \$PATH"
fi

# Honor NUB_NO_MODIFY_PATH: skip all shell-profile edits and just print the line
# the user should add themselves (rustup/uv convention). Runs before the
# already-in-PATH check so the opt-out is unconditional.
case "$(printf '%s' "${NUB_NO_MODIFY_PATH:-}" | tr '[:upper:]' '[:lower:]')" in
    ''|0|no|false|off) ;;
    1|yes|true|on)
        echo "Add the nub bin path to your shell profile:"
        echo -e "  ${Bold}${posix_path_line}${Color_Off}"
        exit 0
        ;;
    *) error "Invalid NUB_NO_MODIFY_PATH: ${NUB_NO_MODIFY_PATH} (expected 1/yes/true/on or 0/no/false/off)" ;;
esac

# Check if already in PATH
if echo "$PATH" | tr ':' '\n' | grep -qx "$bin_dir"; then
    success "Already in PATH. Run: nub --version"
    exit 0
fi

refresh_command=""

case $(basename "${SHELL:-bash}") in
zsh)
    config="$HOME/.zshrc"
    if [[ -w "$config" ]] || [[ ! -f "$config" ]]; then
        {
            echo ''
            echo '# nub'
            echo "$posix_path_line"
        } >> "$config"
        info "Added ${tilde_bin_dir} to \$PATH in ~/.zshrc"
        refresh_command="exec \$SHELL"
    fi
    ;;
bash)
    config=""
    for f in "$HOME/.bashrc" "$HOME/.bash_profile"; do
        if [[ -w "$f" ]]; then config="$f"; break; fi
    done
    if [[ -n "$config" ]]; then
        {
            echo ''
            echo '# nub'
            echo "$posix_path_line"
        } >> "$config"
        info "Added ${tilde_bin_dir} to \$PATH in $(tildify "$config")"
        refresh_command="source $(tildify "$config")"
    fi
    ;;
fish)
    config="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
    if [[ -w "$config" ]] || [[ ! -f "$config" ]]; then
        mkdir -p "$(dirname "$config")"
        {
            echo ''
            echo '# nub'
            echo "$fish_path_line"
        } >> "$config"
        info "Added ${tilde_bin_dir} to \$PATH in $(tildify "$config")"
        refresh_command="source $(tildify "$config")"
    fi
    ;;
*)
    echo "Manually add to your shell config:"
    echo -e "  ${Bold}${posix_path_line}${Color_Off}"
    ;;
esac

echo ""
info "To get started, run:"
echo ""
if [[ -n "$refresh_command" ]]; then
    echo -e "  ${Bold}${refresh_command}${Color_Off}"
fi
echo -e "  ${Bold}nub --version${Color_Off}"
echo ""
