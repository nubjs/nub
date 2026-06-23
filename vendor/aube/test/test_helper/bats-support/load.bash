# Preserve path at the time this file was sourced
# This prevents using of user-defined mocks/stubs that modify the PATH

# BATS_SAVED_PATH was introduced in bats-core v1.10.0
# if it is already set, we can use its more robust value
# else we try to recreate it here
BATS_SAVED_PATH="${BATS_SAVED_PATH-$PATH}"

source "$(dirname "${BASH_SOURCE[0]}")/src/output.bash"
source "$(dirname "${BASH_SOURCE[0]}")/src/error.bash"
source "$(dirname "${BASH_SOURCE[0]}")/src/lang.bash"
