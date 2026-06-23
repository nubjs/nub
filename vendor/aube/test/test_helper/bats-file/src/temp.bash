#
# bats-file - Common filesystem assertions and helpers for Bats
#
# Written in 2016 by Zoltan Tombol <zoltan dot tombol at gmail dot com>
#
# To the extent possible under law, the author(s) have dedicated all
# copyright and related and neighboring rights to this software to the
# public domain worldwide. This software is distributed without any
# warranty.
#
# You should have received a copy of the CC0 Public Domain Dedication
# along with this software. If not, see
# <http://creativecommons.org/publicdomain/zero/1.0/>.
#

#
# temp.bash
# ---------
#
# Functions for handling temporary directories.
#

# Create a temporary directory for the current test in `BATS_TMPDIR`,
# and display its path on the standard output.
#
# The directory name is derived from the test's filename and number, and
# a random string for uniqueness.
#
#   <test-filename>-<test-number>-<random-string>
#
# When `--prefix <prefix>' is specified, `<prefix>' is prepended to the
# directory name.
#
#   <prefix><test-filename>-<test-number>-<random-string>
#
# Must be called from `setup', `@test' or `teardown'.
#
# Example:
#
#   setup() {
#     TEST_TEMP_DIR="$(temp_make --prefix 'myapp-')"
#   }
#
#   teardown() {
#     temp_del "$TEST_TEMP_DIR"
#   }
#
# Globals:
#   BATS_TEST_NAME
#   BATS_TEST_FILENAME
#   BATS_TEST_NUMBER
#   BATS_TMPDIR
# Arguments:
#   none
# Options:
#   -p, --prefix <prefix> - prefix the directory name with `<prefix>'
# Returns:
#   0 - on success
#   1 - otherwise
# Outputs:
#   STDOUT - path of temporary directory
#   STDERR - error messages
temp_make() {
  # Check caller.
  if ! ( batslib_is_caller --indirect 'setup' \
      || batslib_is_caller --indirect 'setup_file' \
      || batslib_is_caller --indirect "$BATS_TEST_NAME" \
      || batslib_is_caller --indirect 'teardown' \
      || batslib_is_caller --indirect 'teardown_file' )
  then
    echo "Must be called from \`setup', \`@test' or \`teardown'" \
      | batslib_decorate 'ERROR: temp_make' \
      | fail
    return $?
  fi

  # Handle options.
  local prefix=''

  while (( $# > 0 )); do
    case "$1" in
      -p|--prefix)
        if (( $# < 2 )); then
          echo "\`--prefix' requires an argument" \
            | batslib_decorate 'ERROR: temp_make' \
            | fail
          return $?
        fi
        prefix="$2"
        shift 2
        ;;
      --) shift; break ;;
      *) break ;;
    esac
  done

  # Create directory.
  local template="$prefix"
  template+="${BATS_TEST_FILENAME##*/}"
  template+="-${BATS_TEST_NUMBER}"
  template+='-XXXXXX'

  local path
  if ! path="$(mktemp -d  --  "${BATS_TMPDIR}/${template}" 2>&1)"; then
    echo "$path" \
      | batslib_decorate 'ERROR: temp_make' \
      | fail
    return $?
  fi

  echo "$path"
}

# Delete a temporary directory, typically created with `temp_make', and
# its contents.
#
# Note: Actually, this function can be used to delete any file or
#       directory. However, it is most useful in deleting temporary
#       directories created with `temp_make', hence the naming.
#
# For development and debugging, deletion can be prevented using
# environment variables.
#
# When `BATSLIB_TEMP_PRESERVE' is set to 1, the function succeeds but
# the directory is not deleted.
#
# When `BATSLIB_TEMP_PRESERVE_ON_FAILURE' is set to 1 and `temp_del' is
# called, directly or indirectly, from `teardown', the function succeeds
# but the directory is not deleted if the test has failed.
#
# Example:
#
#   setup() {
#     TEST_TEMP_DIR="$(temp_make --prefix 'myapp-')"
#   }
#
#   teardown() {
#     temp_del "$TEST_TEMP_DIR"
#   }
#
# Globals:
#   BATSLIB_TEMP_PRESERVE
#   BATSLIB_TEMP_PRESERVE_ON_FAILURE
#   BATS_TEST_COMPLETED
# Arguments:
#   $1 - path of directory
# Returns:
#   0 - on success
#   1 - otherwise
# Outputs:
#   STDERR - error messages
temp_del() {
  local -r path="$1"

  # Environment variables.
  if [[ ${BATSLIB_TEMP_PRESERVE-} == '1' ]]; then
    return 0
  elif [[ ${BATSLIB_TEMP_PRESERVE_ON_FAILURE-} == '1' ]]; then
    # Check caller.
    if ! ( batslib_is_caller --indirect 'teardown' \
        || batslib_is_caller --indirect 'teardown_file' )
    then
      echo "Must be called from \`teardown' or \`teardown_file' when using \`BATSLIB_TEMP_PRESERVE_ON_FAILURE'" \
        | batslib_decorate 'ERROR: temp_del' \
        | fail
      return $?
    fi

    (( ${BATS_TEST_COMPLETED:-0} != 1 )) && return 0
  fi

  # Delete directory.
  local result
  if ! result="$(rm -r -- "$path" 2>&1 >/dev/null )"; then
    echo "$result" \
      | batslib_decorate 'ERROR: temp_del' \
      | fail
    return $?
  fi
}
