#!/usr/bin/env bash
set -euo pipefail

# Nub installer — downloads the latest release binary from GitHub.
# Usage: curl -fsSL https://raw.githubusercontent.com/nubjs/nub/main/install.sh | bash

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
    version=$(curl -fsSL "https://api.github.com/repos/nubjs/nub/releases/latest" | grep '"tag_name"' | sed -E 's/.*"v(.*)".*/\1/')
    if [[ -z "$version" ]]; then
        error "Failed to determine latest version"
    fi
fi

# --- Install ---

install_dir="$HOME/.nub"
bin_dir="$install_dir/bin"
exe="$bin_dir/nub"

info "Installing nub v${version} for ${target}..."

mkdir -p "$bin_dir" || error "Failed to create install directory: $bin_dir"

# Download the per-platform archive (binary + runtime) and extract it into the
# install dir. The archive ships bin/nub alongside runtime/ (preload.mjs +
# vendored node_modules); without runtime/, nub cannot transpile at all (A30).
# (Windows is handled by install.ps1 above, so $target is always darwin/linux.)
url="https://github.com/nubjs/nub/releases/download/v${version}/nub-${target}.tar.gz"

tmp_archive=$(mktemp) || error "Failed to create temp file"
trap 'rm -f "$tmp_archive"' EXIT

curl --fail --location --progress-bar --output "$tmp_archive" "$url" ||
    error "Failed to download nub from: $url"

# Replace any prior bin/ + runtime/ for a clean upgrade (other files under
# $install_dir, e.g. caches, are preserved), then extract bin/ + runtime/.
rm -rf "${install_dir:?}/bin" "${install_dir:?}/runtime"
tar -xzf "$tmp_archive" -C "$install_dir" ||
    error "Failed to extract nub archive from: $url"

[[ -f "$exe" ]] || error "Archive did not contain bin/nub"
chmod +x "$exe" || error "Failed to set permissions on $exe"

success "Installed nub v${version} to $exe"

# --- PATH setup ---

tildify() {
    if [[ $1 == "$HOME"/* ]]; then
        echo "~${1#$HOME}"
    else
        echo "$1"
    fi
}

tilde_bin_dir=$(tildify "$bin_dir")

# PATH export lines reference $HOME so they stay portable across machines.
posix_path_line='export PATH="$HOME/.nub/bin:$PATH"'
fish_path_line='set -gx PATH $HOME/.nub/bin $PATH'

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
