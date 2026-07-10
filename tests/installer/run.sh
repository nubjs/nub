#!/usr/bin/env bash

set -e # errexit
set -u # nounset

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

    printf '%s' "$dir"
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
}

test_begin 'install for Bash shell'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir)

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
    grep -q -F 'set --global --export PATH "$HOME/.nub/bin" $PATH' "$HOME/.config/fish/config.fish" \
        || throw 'shell configuration not found [2]' "$HOME/.config/fish/config.fish"
)
test_end $?

test_begin 'install for Dash shell'
(
    set -e
    export SHELL=/bin/dash
    export HOME=$(mksandboxdir)

    output=$(test_install "$HOME/.nub")

    { printf '%s' "$output" | grep -q -F 'export PATH="$HOME/.nub/bin:$PATH"'; } \
        || throw 'shell configuration not emitted in output [1]' "$output"
)
test_end $?

test_begin 'install without creating shell configuration'
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

test_begin 'install without changing shell configuration'
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

test_begin 'install for Bash with custom installation dir'
(
    set -e
    export SHELL=/bin/bash
    export HOME=$(mksandboxdir home)
    export NUB_INSTALL_DIR=$(mksandboxdir install)

    test_install "$NUB_INSTALL_DIR"

    grep -q -F '# nub' "$HOME/.bashrc" \
        || throw 'shell configuration not found [1]' "$HOME/.bashrc"
    grep -q -F "export PATH=\"$NUB_INSTALL_DIR/bin:\$PATH\"" "$HOME/.bashrc" \
        || throw 'shell configuration not found [2]' "$HOME/.bashrc"
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
