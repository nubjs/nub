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

default_install_dir=$(cd "$HOME" && pwd)/.nub
install_dir=${NUB_INSTALL_DIR:-$default_install_dir}
mkdir -p "$install_dir" || error "Failed to create install directory: $install_dir"
install_dir=$(cd "$install_dir" && pwd) || error "Invalid NUB_INSTALL_DIR: $install_dir"
install_bin_dir="$install_dir/bin"
install_exe="$install_bin_dir/nub"

info "Installing nub v${version} for ${target}..."

mkdir -p "$install_bin_dir" || error "Failed to create install directory: $install_bin_dir"

# Download the per-platform archive and extract it into the install dir. nub is a
# single self-contained binary that embeds its runtime (preload + vendored
# node_modules + native addon) and JIT-extracts it to ~/.cache/nub on first run.
# The archive ships bin/ plus a vestigial empty runtime/ (kept only to satisfy the
# sidecar-era `nub upgrade`; the binary ignores ~/.nub/runtime — see release.yml).
# (Windows is handled by install.ps1 above, so $target is always darwin/linux.)
url="https://github.com/nubjs/nub/releases/download/v${version}/nub-${target}.tar.gz"

tmp_archive=$(mktemp) || error "Failed to create temp file"
trap 'rm -f "$tmp_archive"' EXIT

curl --fail --location --progress-bar --output "$tmp_archive" "$url" ||
    error "Failed to download nub from: $url"

# Replace any prior bin/ for a clean upgrade (other files under $install_dir,
# e.g. caches, are preserved). A stale runtime/ from a pre-single-binary install
# is also removed so it can't shadow the embedded runtime. Then extract bin/.
if test "$install_dir" = "$default_install_dir"; then
    rm -rf "$install_dir/bin" "$install_dir/runtime"
else
    rm -f "$install_dir/bin/nub" "$install_dir/bin/nubx"
fi
tar -xzf "$tmp_archive" -C "$install_dir" ||
    error "Failed to extract nub archive from: $url"

[[ -f "$install_exe" ]] || error "Archive did not contain bin/nub"
chmod +x "$install_exe" || error "Failed to set permissions on $install_exe"

# `nubx` is the same binary as `nub`, dispatched on argv[0] (cli.rs reads
# args_os()[0].file_stem(): "nubx" -> exec). The release archive ships only
# bin/nub, so create the nubx alias as a relative symlink alongside it. `-f`
# makes this idempotent across reinstall/upgrade and harmless if a future
# archive ever ships its own nubx. Relative target keeps it valid if ~/.nub moves.
ln -sf nub "$install_bin_dir/nubx" || error "Failed to create nubx symlink in $install_bin_dir"

success "Installed nub v${version} (with nubx) to $install_exe"

# --- PATH setup ---

configure_shell_path() {
    local bin_dir=$1

    local shell_name=$(basename "${SHELL:-bash}")

    case "$shell_name" in
        bash) configure_shell_path__posix "$bin_dir" "$HOME/.bashrc" "$HOME/.bash_profile";;
        zsh) configure_shell_path__posix "$bin_dir" "$HOME/.zshrc" "$HOME/.zshenv";;
        fish) configure_shell_path__fish "$bin_dir";;
        *) configure_shell_path__unknown "$bin_dir";;
    esac
}
configure_shell_path__posix() {
    local bin_dir=$1
    shift

    local shell_file="$1"

    for file in "$@"; do
        if test -w "$file"; then
            shell_file=$file
            break
        fi
    done

    if test -f "$shell_file" && test ! -w "$shell_file"; then
        return
    fi

    sed "s/^$(printf '%8s')//" <<END >> "$shell_file"

        # nub
        $(print_extend_path__posix "$bin_dir")
END

    RefreshCommand=". $(replace_home_path_with_var "$shell_file")"

    local tilde_bin_dir=$(replace_home_path_with_tilde "$bin_dir")
    local tilde_shell_file=$(replace_home_path_with_tilde "$shell_file")

    info "Added $tilde_bin_dir to \$PATH in $tilde_shell_file"
}
configure_shell_path__fish() {
    local bin_dir=$1

    local shell_file=${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish

    for file in "${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"; do
        if test -w "$file"; then
            shell_file=$file
            break
        fi
    done

    if test -f "$shell_file" && test ! -w "$shell_file"; then
       return
    fi

    mkdir -p "$(dirname "$shell_file")"

    local bin_dir_portable=$(replace_home_path_with_var "$bin_dir")

    sed "s/^$(printf '%8s')//" <<END >> "$shell_file"

        # nub
        set --global --export PATH "$bin_dir_portable" \$PATH
END

    RefreshCommand="source $(replace_home_path_with_var "$shell_file")"

    local tilde_bin_dir=$(replace_home_path_with_tilde "$bin_dir")
    local tilde_shell_file=$(replace_home_path_with_tilde "$shell_file")

    info "Added $tilde_bin_dir to \$PATH in $tilde_shell_file"
}
configure_shell_path__unknown() {
    local bin_dir=$1

    echo 'Please add the nub bin path to your shell configuration:'
    echo -e "  ${Bold}$(print_extend_path__posix "$bin_dir")${Color_Off}"
}

print_extend_path__posix() {
    local dir=$1

    local dir_portable=$(replace_home_path_with_var "$dir")

    printf 'export PATH="%s:$PATH"' "$dir_portable"
}

replace_home_path_with_var() {
    # We replace home path with $HOME variable reference, so path stays portable across machines.
    local path=$1

    case "$path" in
        $HOME/*) printf '%s' "\$HOME/${path#"$HOME"/}";;
        *) printf '%s' "$path";;
    esac
}
replace_home_path_with_tilde() {
    # We replace home path with ~ reference, so path stays portable across machines.
    local path=$1

    case "$path" in
        $HOME/*) printf '%s' "~/${path#"$HOME"/}";;
        *) printf '%s' "$path";;
    esac
}

no_shell_path=$(printf '%s' "${NUB_NO_MODIFY_PATH:-0}" | tr '[:upper:]' '[:lower:]')

case "$no_shell_path" in
    0|no|false|off) ;;
    1|yes|true|on) configure_shell_path__unknown "$install_bin_dir"; exit;;
    *) error "Invalid NUB_NO_MODIFY_PATH: $NUB_NO_MODIFY_PATH";;
esac

# Check if already in PATH.
if echo "$PATH" | tr ':' '\n' | grep -qxF "$install_bin_dir"; then
    success "Already in PATH. Run: nub --version"
    exit
fi

RefreshCommand=''

configure_shell_path "$install_bin_dir"

echo ''
info 'To get started, run:'
echo ''
if [[ -n "$RefreshCommand" ]]; then
    echo -e "  ${Bold}${RefreshCommand}${Color_Off}"
fi
echo -e "  ${Bold}nub --version${Color_Off}"
echo ''
