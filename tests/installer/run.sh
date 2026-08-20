#!/usr/bin/env bash

set -e # errexit
set -u # nounset

# Local end-to-end harness for install.sh. Each case runs the REAL installer
# against a throwaway HOME/NUB_INSTALL_DIR sandbox (so it downloads the actual
# latest release from GitHub — network required) and asserts the resulting on-disk
# state: the binaries, the `.nub-receipt` marker, and the shell-profile edits for
# each shell + the NUB_INSTALL_DIR / NUB_NO_MODIFY_PATH knobs. Cross-OS coverage of
# the same paths runs in CI via .github/workflows/verify-install.yml.
#
# Usage: tests/installer/run.sh   (set TEST_CLEAN=0 to keep the sandbox for debugging)

# Required on MacOS due to a Bash 3.2.57 bug.
case "$(dirname "$0")" in
    /*|./*) Dir=$(cd "$(dirname "$0")" && pwd);;
    *) Dir=$(cd "$PWD/$(dirname "$0")" && pwd);;
esac

ResetStyle=
BoldStyle=
ColorBlue=
ColorGray=
ColorGreen=
ColorRed=
if \
    test -t 1 \
    && ! test -p /dev/stdout \
    && test -z "${NO_COLOR:-}" \
    || test "${FORCE_COLOR:-0}" = 1 \
; then
    ResetStyle='\033[0m'
    BoldStyle='\033[1m'
    ColorBlue='\033[36m'
    ColorGray='\033[30m'
    ColorGreen='\033[32m'
    ColorRed='\033[31m'
fi

test_idx=0
test_success_idx=0
test_failure_idx=0
test_sandbox_dir=$(mktemp -d)

clean() {
    if test ${TEST_CLEAN:-1} -eq 0; then
        return
    fi
    if test -d "$test_sandbox_dir"; then
        rm -rf "$test_sandbox_dir"
    fi
}

trap clean EXIT

describe() {
    test_idx=$(expr $test_idx + 1)
    echo
    printf "${ResetStyle}${BoldStyle}${ColorBlue}[test]${ResetStyle} ${ColorBlue}%s${ResetStyle}\n" "$*"
}
throw() {
    printf "${ResetStyle}${BoldStyle}${ColorRed}[error]${ResetStyle} ${ColorRed}%s${ResetStyle}\n" "$*"
    exit 1
}
success() {
    test_success_idx=$(expr $test_success_idx + 1)
    printf "${ResetStyle}${BoldStyle}${ColorGreen}[done]${ResetStyle}\n"
    echo
}
failure() {
    test_failure_idx=$(expr $test_failure_idx + 1)
    printf "${ResetStyle}${BoldStyle}${ColorRed}[failed]${ResetStyle}\n"
    echo
}
mksandboxdir() {
    local dir="$test_sandbox_dir/$test_idx${1:+/$1}"

    mkdir -p "$dir"

    # Emit the normalized (symlink-resolved) path so callers compare against the
    # exact string install.sh derives via `cd … && pwd` (macOS' /var → /private/var).
    (cd "$dir" && pwd)
}
test_begin() {
    describe "$@"
    set +e
}
test_end() {
    set -e
    if test $1 -eq 0; then
        success
    else
        failure
    fi
}

# Runs the installer (inheriting the caller's env) and asserts the install
# artifacts landed in $1: both binaries and the self-managed-install receipt.
test_install() {
    local dir=$1

    "$Dir/../../install.sh" \
        || throw 'installation failed' "$dir"

    test -d "$dir" \
        || throw 'directory does not exist' "$dir"
    test -f "$dir/bin/nub" \
        || throw 'file does not exist' "$dir/bin/nub"
    test -f "$dir/bin/nubx" \
        || throw 'file does not exist' "$dir/bin/nubx"
    test -f "$dir/.nub-receipt" \
        || throw 'install receipt not written' "$dir/.nub-receipt"
}

test_begin 'install for Bash shell'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir)

    # bash appends only to an EXISTING writable rc (unchanged from the shipped
    # script; the zsh/fish branches create theirs). A real bash user has one.
    touch "$HOME/.bashrc"
    test_install "$HOME/.nub"

    grep -q -F '# nub' "$HOME/.bashrc" \
        || throw 'shell configuration not found [1]' "$HOME/.bashrc"
    grep -q -F 'export PATH="$HOME/.nub/bin:$PATH"' "$HOME/.bashrc" \
        || throw 'shell configuration not found [2]' "$HOME/.bashrc"
)
test_end $?

test_begin 'install for ZSH shell'
(
    set -e
    export SHELL=/bin/zsh
    export HOME=$(mksandboxdir)

    test_install "$HOME/.nub"

    grep -q -F '# nub' "$HOME/.zshrc" \
        || throw 'shell configuration not found [1]' "$HOME/.zshrc"
    grep -q -F 'export PATH="$HOME/.nub/bin:$PATH"' "$HOME/.zshrc" \
        || throw 'shell configuration not found [2]' "$HOME/.zshrc"
)
test_end $?

test_begin 'install for Fish shell'
(
    set -e
    export SHELL=/bin/fish
    export HOME=$(mksandboxdir)
    unset XDG_CONFIG_HOME

    test_install "$HOME/.nub"

    grep -q -F '# nub' "$HOME/.config/fish/config.fish" \
        || throw 'shell configuration not found [1]' "$HOME/.config/fish/config.fish"
    grep -q -F 'set -gx PATH "$HOME/.nub/bin" $PATH' "$HOME/.config/fish/config.fish" \
        || throw 'shell configuration not found [2]' "$HOME/.config/fish/config.fish"
)
test_end $?

test_begin 'install for Dash shell (unknown shell prints the manual line)'
(
    set -e
    export SHELL=/bin/dash
    export HOME=$(mksandboxdir)

    output=$(test_install "$HOME/.nub")

    { printf '%s' "$output" | grep -q -F 'export PATH="$HOME/.nub/bin:$PATH"'; } \
        || throw 'shell configuration not emitted in output [1]' "$output"
)
test_end $?

test_begin 'install with NUB_NO_MODIFY_PATH does not create a shell profile'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir)
    export NUB_NO_MODIFY_PATH=1

    test_install "$HOME/.nub"

    ! test -f "$HOME/.bashrc" \
        || throw 'shell configuration file created' "$HOME/.bashrc"
)
test_end $?

test_begin 'install with NUB_NO_MODIFY_PATH does not alter an existing shell profile'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir)
    export NUB_NO_MODIFY_PATH=true

    touch "$HOME/.bashrc"
    test_install "$HOME/.nub"

    ! grep -q -F '# nub' "$HOME/.bashrc" \
        || throw 'shell configuration altered [1]' "$HOME/.bashrc"
)
test_end $?

test_begin 'install with NUB_INSTALL_DIR relocates the install and PATH line'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir home)
    export NUB_INSTALL_DIR=$(mksandboxdir install)

    touch "$HOME/.bashrc"
    test_install "$NUB_INSTALL_DIR"

    grep -q -F '# nub' "$HOME/.bashrc" \
        || throw 'shell configuration not found [1]' "$HOME/.bashrc"
    # An out-of-home custom dir is emitted as its absolute (normalized) path.
    grep -q -F "export PATH=\"$NUB_INSTALL_DIR/bin:\$PATH\"" "$HOME/.bashrc" \
        || throw 'shell configuration not found [2]' "$HOME/.bashrc"
)
test_end $?

test_begin 'install for Fish with a custom dir containing a space (quoted PATH line)'
(
    set -e
    export SHELL=/bin/fish
    export HOME=$(mksandboxdir home)
    unset XDG_CONFIG_HOME
    export NUB_INSTALL_DIR="$(mksandboxdir 'my tools')/nub"

    test_install "$NUB_INSTALL_DIR"

    conf="$HOME/.config/fish/config.fish"
    # The dir must be quoted or fish word-splits the space into two PATH entries.
    grep -q -F "set -gx PATH \"$NUB_INSTALL_DIR/bin\" \$PATH" "$conf" \
        || throw 'fish PATH line not quoted for a spaced dir' "$conf"
)
test_end $?

# Every case above installs into an EMPTY sandbox, which is why this whole class of
# upgrade-over-an-existing-install defect was invisible here. `nub pm shim` hardlinks
# the shim dir's <pm> at bin/nub; re-running the installer replaces bin/nub with a
# new inode, so without a refresh those links keep serving the PREVIOUS version —
# no error, no version warning.
#
# This case covers the PRE-MOVE location (`~/.nub/shims`). The shim dir moved to
# ${XDG_DATA_HOME:-$HOME/.local/share}/nub/shims, and install.sh still refreshes
# the old one so an install predating the move keeps working until the user's next
# `nub pm shim` migrates it.
test_begin 'reinstall re-links PRE-MOVE PM shims and creates no new ones'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir)
    # CONTAINMENT, not tidiness. refresh_pm_shims sweeps
    # ${XDG_DATA_HOME:-$HOME/.local/share}/nub/shims, expanded at run time — so if
    # the invoking shell exports XDG_DATA_HOME to a real path outside this
    # sandbox, the case would `ln -f` the DEVELOPER'S live npm/yarn shims onto the
    # throwaway binary installed below, which `trap clean EXIT` then deletes.
    # Pinning it inside $HOME keeps every write in the sandbox.
    export XDG_DATA_HOME="$HOME/.local/share"

    touch "$HOME/.bashrc"
    test_install "$HOME/.nub"

    # Stands in for `nub pm shim`, which cannot run here — it would need the binary
    # on PATH and writes to the real ~/.nub. Only two of the six names, so the
    # refresh-only contract is testable: the other four must NOT appear afterwards.
    mkdir -p "$HOME/.nub/shims"
    ln "$HOME/.nub/bin/nub" "$HOME/.nub/shims/npm"
    ln "$HOME/.nub/bin/nub" "$HOME/.nub/shims/yarn"

    "$Dir/../../install.sh" \
        || throw 'reinstall failed' "$HOME/.nub"

    # `-ef` is same-device-and-inode, which is exactly the property the refresh has
    # to restore. A size or mtime comparison would pass on a stale link to an
    # identically-sized old binary.
    test "$HOME/.nub/shims/npm" -ef "$HOME/.nub/bin/nub" \
        || throw 'shim still points at the pre-upgrade binary' "$HOME/.nub/shims/npm"
    test "$HOME/.nub/shims/yarn" -ef "$HOME/.nub/bin/nub" \
        || throw 'shim still points at the pre-upgrade binary' "$HOME/.nub/shims/yarn"

    for absent in npx pnpm pnpx yarnpkg; do
        ! test -e "$HOME/.nub/shims/$absent" \
            || throw 'installer created a shim the user never opted into' "$HOME/.nub/shims/$absent"
    done

    ! test -e "$HOME/.nub/shims.lock" \
        || throw 'shim lock left behind' "$HOME/.nub/shims.lock"
)
test_end $?

# The LIVE location. `nub pm shim` puts the dir under $XDG_DATA_HOME (or its
# ~/.local/share default) — resolve_shim_dir's one rule — and a refresher that
# misses it is SILENT: the shims keep executing the pre-upgrade inode with no
# error at all, which is the failure this whole leg exists to prevent.
test_begin 'reinstall re-links PM shims at the live XDG location'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir)
    export XDG_DATA_HOME="$HOME/xdg-data"

    touch "$HOME/.bashrc"
    test_install "$HOME/.nub"

    # No ~/.nub/shims here: this is the fresh-install shape, where the shims live
    # under XDG alone. Two of six names again, so refresh-only stays testable.
    mkdir -p "$XDG_DATA_HOME/nub/shims"
    ln "$HOME/.nub/bin/nub" "$XDG_DATA_HOME/nub/shims/npm"
    ln "$HOME/.nub/bin/nub" "$XDG_DATA_HOME/nub/shims/yarn"

    "$Dir/../../install.sh" \
        || throw 'reinstall failed' "$HOME/.nub"

    test "$XDG_DATA_HOME/nub/shims/npm" -ef "$HOME/.nub/bin/nub" \
        || throw 'XDG shim still points at the pre-upgrade binary' "$XDG_DATA_HOME/nub/shims/npm"
    test "$XDG_DATA_HOME/nub/shims/yarn" -ef "$HOME/.nub/bin/nub" \
        || throw 'XDG shim still points at the pre-upgrade binary' "$XDG_DATA_HOME/nub/shims/yarn"

    for absent in npx pnpm pnpx yarnpkg; do
        ! test -e "$XDG_DATA_HOME/nub/shims/$absent" \
            || throw 'installer created a shim the user never opted into' "$XDG_DATA_HOME/nub/shims/$absent"
    done

    # The lock sits beside the dir it guards (ShimLock::acquire), so the XDG leg's
    # lock is under XDG — not $HOME/.nub — and must be cleaned up there.
    ! test -e "$XDG_DATA_HOME/nub/shims.lock" \
        || throw 'shim lock left behind' "$XDG_DATA_HOME/nub/shims.lock"

    # The legacy dir must not be conjured into existence by the sweep.
    ! test -e "$HOME/.nub/shims" \
        || throw 'installer created a legacy shim dir the user never opted into' "$HOME/.nub/shims"
)
test_end $?

echo
if test $test_failure_idx -eq 0; then
    printf "${ColorGray}%3s failed${ResetStyle}\n" $test_failure_idx
    printf "${ColorGreen}%3s succeeded${ResetStyle}\n" $test_success_idx
    printf "${BoldStyle}${ColorGreen}TESTS SUCCEEDED${ResetStyle}\n"
else
    printf "${ColorRed}%3s failed${ResetStyle}\n" $test_failure_idx
    printf "${ColorGray}%3s succeeded${ResetStyle}\n" $test_success_idx
    printf "${BoldStyle}${ColorRed}TESTS FAILED${ResetStyle}\n"
fi

exit $test_failure_idx
