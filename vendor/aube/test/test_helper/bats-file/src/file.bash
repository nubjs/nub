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
# file.bash
# ---------
#
# Assertions are functions that perform a test and output relevant
# information on failure to help debugging. They return 1 on failure
# and 0 otherwise.
#
# All output is formatted for readability using the functions of
# `output.bash' and sent to the standard error.
#

# Fail and display path of the file or directory if it does not exist.
# This function is the logical complement of `assert_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - file or directory exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_exists() {
  local -r file="$1"
  if [[ ! -e "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file or directory does not exist' \
      | fail
  fi
}

# Fail and display path of the file if it does not exist.
# This function is the logical complement of `assert_file_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - file exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_exists() {
  local -r file="$1"
  if [[ ! -f "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file does not exist' \
      | fail
  fi
}

# Fail and display path of the directory if it does not exist.
# This function is the logical complement of `assert_dir_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - directory exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_dir_exists() {
  local -r file="$1"
  if [[ ! -d "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'directory does not exist' \
      | fail
  fi
}

# Fail and display path of the block special file if it does not exist.
# This function is the logical complement of `assert_block_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - block special file exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_block_exists() {
  local -r file="$1"
  if [[ ! -b "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'block special file does not exist' \
      | fail
  fi
}

# Fail and display path of the character special file if it does not exist.
# This function is the logical complement of `assert_character_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - character special file exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_character_exists() {
  local -r file="$1"
  if [[ ! -c "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'character special file does not exist' \
      | fail
  fi
}

# Fail and display path of the symbolic link if it does not exist.
# This function is the logical complement of `assert_link_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - symbolic link exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_link_exists() {
  local -r file="$1"
  if [[ ! -L "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'symbolic link does not exist' \
      | fail
  fi
}

# Fail and display path of the socket if it does not exist.
# This function is the logical complement of `assert_socket_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - socket exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_socket_exists() {
  local -r file="$1"
  if [[ ! -S "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'socket does not exist' \
      | fail
  fi
}

# Fail and display path of the named pipe if it does not exist.
# This function is the logical complement of `assert_fifo_not_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - named pipe exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_fifo_exists() {
  local -r file="$1"
  if [[ ! -p "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'fifo does not exist' \
      | fail
  fi
}

# Fail and display path of the named file if it is not executable.
# This function is the logical complement of `assert_file_not_executable'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - named pipe exists
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_executable() {
  local -r file="$1"
  if [[ ! -x "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file is not executable' \
      | fail
  fi
}

# Fail and display path of both files if they are not equal.
# This function is the logical complement of `assert_files_not_equal'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - 1st path
#   $2 - 2nd path
# Returns:
#   0 - named files are the same
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_files_equal() {
  local -r file1="$1"
  local -r file2="$2"
  if ! cmp -s "$file1" "$file2" >/dev/null ; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file1/$rem/$add}" 'path' "${file2/$rem/$add}" \
      | batslib_decorate 'files are not the same' \
      | fail
  fi
}

# Get the owner of a file
# Arguments:
# $1 - output variable name
# $2 - path to file
_bats_get_file_owner() {
  local -r output_var=$1
  local -r file=$2
  if [[ "$OSTYPE" == darwin* ]]; then
      local -ra cmd_params=(-f %Su)
  else
      local -ra cmd_params=(-c %U)
  fi
  printf -v "$output_var" "%s" "$(stat "${cmd_params[@]}" "$file")"
}

# Fail and display path of the user is not the owner of a file. This
# function is the logical complement of `assert_file_not_owner'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - owns file
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_owner() {
  local -r owner="$1"
  local -r file="$2"

  local actual_owner
  _bats_get_file_owner actual_owner "$file"
  readonly actual_owner

  if [[ "$actual_owner" != "$owner" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate "user $owner is not the owner of the file" \
      | fail
  fi
}

# Fail if file does not have given permissions. This
# function is the logical complement of `assert_file_not_permission'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - permission
#   $2 - path
# Returns:
#   0 - file has given permissions
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_permission() {
  local -r permission="$1"
  local -r file="$2"

  if [[ "$OSTYPE" == darwin* ]]; then
      local -r actual_permission=$(stat -f '%A' "$file")
  else
      local -r actual_permission=$(stat -c "%a" "$file")
  fi

  if [[ "$actual_permission" != "$permission" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate "file does not have permissions $permission" \
      | fail
  fi
}

# Fail if file is not zero byte. This
# function is the logical complement of `assert_size_not_zero'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - file size is zero byte
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_size_zero() {
  local -r file="$1"

  if [ -s "$file" ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file is greater than 0 byte' \
      | fail
  fi
}

# Fail if group if is not set on file. This
# function is the logical complement of `assert_file_not_group_id_set'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - group id is set
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_group_id_set() {
  local -r file="$1"
  if [[ ! -g "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'set-group-ID is not set' \
      | fail
  fi
}

# Fail if user if is not set on file. This
# function is the logical complement of `assert_file_not_user_id_set'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - user id is set
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_user_id_set() {
  local -r file="$1"
  if [[ ! -u "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'set-user-ID is not set' \
      | fail
  fi
}

# Fail if stickybit not set on file. This
# function is the logical complement of `assert_sticky_bit'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - stickybit is set
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_sticky_bit() {
  local -r file="$1"
  if [[ ! -k "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'stickybit is not set' \
      | fail
  fi
}

_bats_file_readlinkf_macos() {
  local TARGET_FILE=$1
  cd "$(dirname "$TARGET_FILE")" 2>/dev/null || return
  TARGET_FILE=$(basename "$TARGET_FILE")
  # Iterate down a (possible) chain of symlinks
  while [ -L "$TARGET_FILE" ]
  do
    TARGET_FILE=$(readlink "$TARGET_FILE")
    cd "$(dirname "$TARGET_FILE")" 2>/dev/null || return
    TARGET_FILE=$(basename "$TARGET_FILE")
  done
  # Compute the canonicalized name by finding the physical path
  # for the directory we're in and appending the target file.
  local -r PHYS_DIR=$(pwd -P)

  printf "%s/%s\n" "$PHYS_DIR" "$TARGET_FILE"
}

# Fail and display path of the file (or directory) if it is not a symlink to given destination.
# function is the logical complement of `assert_not_symlink_to`
#   $1 - source
#   $2 - destination
# Returns:
#   0 - link to correct target
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_symlink_to() {
  local -r sourcefile="$1"
  local -r link="$2"

  # If OS is OSX, emulate readlink -f
  if [[ $OSTYPE == "darwin"* ]]; then
    local -ra readlink_command=(_bats_file_readlinkf_macos)
  else
    local -ra readlink_command=(readlink -f)
  fi

  if [ ! -L "$link"   ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${link/$rem/$add}" \
      | batslib_decorate 'file is not a symbolic link' \
      | fail
  fi

  local realsource; realsource=$( "${readlink_command[@]}" "$link" ); readonly realsource
  local realexpectedsource; realexpectedsource=$( "${readlink_command[@]}" "$sourcefile" ); readonly realexpectedsource
  if [ ! "${realsource}" = "${realexpectedsource}"  ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${link/$rem/$add}" \
      | batslib_decorate 'symbolic link does not have the correct target' \
      | fail
  fi
}
# Fail and display path of the file (or directory) if it does not match a size.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
#   $2 - expected size (bytes)
# Returns:
#   0 - file is correct size
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_size_equals() {
  local -r file="$1"
  local -r expectedsize="$2"
  local -r size=$( wc -c "$file" | awk '{print $1}' )
  if [ ! "$expectedsize" = "$size" ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file size does not match expected size' \
      | fail
  fi
}
# Fail and display path of the file (or directory) if it does not contain a string.
# This function is the logical complement of `assert_file_not_contains'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
#   $2 - regex
#   $3 - grep engine to use (grep, egrep, pcregrep) - optional
# Returns:
#   0 - file contains regex
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_contains() {
  local -r file="$1"
  local -r regex="$2"
  local -r cmd="${3:-grep}"

  case "$cmd" in
    grep|egrep|pcregrep)
      if ! type "${cmd}" &>/dev/null; then
        batslib_decorate "Regex engine \"${cmd}\" not available on this system" \
          | fail
      fi
    ;;
    *)
      batslib_decorate "Regex engine \"${cmd}\" not in allow list" \
      | fail
    ;;
  esac
  if ! "$cmd" -q "$regex" "$file"; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" 'regex' "$regex" \
      | batslib_decorate 'file does not contain regex' \
      | fail
  fi
}
# Fail and display path of the file (or directory) if it does contain a string.
# This function is the logical complement of `assert_file_contains'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
#   $2 - regex
# Returns:
#   0 - file does not contain regex
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_not_contains() {
  local -r file="$1"
  local -r regex="$2"

  if [[ ! -f "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" 'regex' "$regex" \
      | batslib_decorate 'file does not exist' \
      | fail

  elif grep -q "$regex" "$file"; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" 'regex' "$regex" \
      | batslib_decorate 'file contains regex' \
      | fail

  fi
}
# Fail and display path of the file (or directory) if it is not empty.
# This function is the logical complement of `assert_file_not_empty'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - file empty
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_empty() {
  local -r file="$1"
  if [[ -s "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    { local -ir width=8
      batslib_print_kv_single "$width" 'path' "${file/$rem/$add}"
      batslib_print_kv_single_or_multi "$width" \
          'output' "$(cat "$file")"
    } | batslib_decorate 'file is not empty' \
      | fail
  fi
}
# Fail and display path of the file (or directory) if it exists. This
# function is the logical complement of `assert_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - file does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_not_exists() {
  local -r file="$1"
  if [[ -e "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file or directory exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the file if it exists. This
# function is the logical complement of `assert_file_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - file does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_not_exists() {
  local -r file="$1"
  if [[ -f "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the directory if it exists. This
# function is the logical complement of `assert_dir_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - directory does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_dir_not_exists() {
  local -r file="$1"
  if [[ -d "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'directory exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the block special file if it exists. This
# function is the logical complement of `assert_block_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - block special file does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_block_not_exists() {
  local -r file="$1"
  if [[ -b "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'block special file exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the character special file if it exists. This
# function is the logical complement of `assert_character_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - character special file does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_character_not_exists() {
  local -r file="$1"
  if [[ -c "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'character special file exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the symbolic link if it exists. This
# function is the logical complement of `assert_link_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - symbolic link does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_link_not_exists() {
  local -r file="$1"
  if [[ -L "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'symbolic link exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the socket if it exists. This
# function is the logical complement of `assert_socket_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - socket does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_socket_not_exists() {
  local -r file="$1"
  if [[ -S "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'socket exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the named pipe if it exists. This
# function is the logical complement of `assert_fifo_exists'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - named pipe does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_fifo_not_exists() {
  local -r file="$1"
  if [[ -p "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'fifo exists, but it was expected to be absent' \
      | fail
  fi
}

# Fail and display path of the named file if it is executable. This
# function is the logical complement of `assert_file_executable'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - named pipe does not exist
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_not_executable() {
  local -r file="$1"
  if [[ -x "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file is executable, but it was expected to be not executable' \
      | fail
  fi
}

# Fail if the user is not the owner of the given file.. This
# function is the logical complement of `assert_file_owner'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - is not an owner
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_not_file_owner() {
  local -r expected_owner="$1"
  local -r file="$2"

  local actual_owner
  _bats_get_file_owner actual_owner "$file"
  readonly actual_owner

  if [[ "$actual_owner" == "$expected_owner" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate "user $expected_owner is the owner of the file, but it was expected not to be" \
      | fail
  fi
}

# Fail if the file has given permissions. This
# function is the logical complement of `assert_file_permission'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - does not have given permissions
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_not_file_permission() {
  local -r permission="$1"
  local -r file="$2"

  if [[ "$OSTYPE" == darwin* ]]; then
      local -r actual_permission=$(stat -f '%A' "$file")
  else
      local -r actual_permission=$(stat -c "%a" "$file")
  fi

  if [ "$actual_permission" -eq "$permission" ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate "file has permissions $permission, but it was expected not to have" \
      | fail
  fi
}

# This function is the logical complement of `assert_files_equal'.
#   $1 - 1st path
#   $2 - 2nd path
# Returns:
#   0 - named files are the same
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_files_not_equal() {
  local -r file1="$1"
  local -r file2="$2"
  if cmp -s "$file1" "$file2" >/dev/null ; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file1/$rem/$add}" 'path' "${file2/$rem/$add}" \
      | batslib_decorate 'files are the same' \
      | fail
  fi
}

# Fail if The file size is zero byte. This
# function is the logical complement of `assert_size_zero'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - size is not 0 byte
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_size_not_zero() {
  local -r file="$1"
  if [[ ! -s "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file is 0 byte, but it was expected not to be' \
      | fail
  fi
}


# Fail if group id is set. This
# function is the logical complement of `assert_file_group_id_set'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - group id is not set
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_not_group_id_set() {
  local -r file="$1"
  if [ -g "$file" ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'group id is set, but it was expected not to be' \
      | fail
  fi
}


# Fail if user id is set. This
# function is the logical complement of `assert_file_user_id_set'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - user id is not set
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_not_user_id_set() {
  local -r file="$1"
  if [ -u "$file" ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'user id is set, but it was expected not to be' \
      | fail
  fi
}

# Fail if stickybit is set. This
# function is the logical complement of `assert_sticky_bit'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - stickybit not set
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_no_sticky_bit() {
  local -r file="$1"
  if [ -k "$file" ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'stickybit is set, but it was expected not to be' \
      | fail
  fi
}

# Fail and display path of the file (or directory) if it is a symlink.
# function is the logical complement of `assert_symlink_to'.
#   $1 - source
#   $2 - destination
# Returns:
#   0 - link to correct target
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_not_symlink_to() {
  local -r sourcefile="$1"
  local -r link="$2"


  if [[ $OSTYPE == darwin* ]]; then
    local -ra readlink_command=(_bats_file_readlinkf_macos)
  else
    local -ra readlink_command=(readlink -f)
  fi

  if [ -L "$link"   ]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${link/$rem/$add}" \
      | batslib_decorate 'file is a symbolic link' \
      | fail
  fi

  local -r realsource=$( "${readlink_command[@]}" "$link" )
  if [ "$realsource" = "$sourcefile"  ]; then
    batslib_print_kv_single 4 'path' "${link/$rem/$add}" \
      | batslib_decorate 'symbolic link does have the correct target' \
      | fail
  fi
}
# Fail and display path of the file (or directory) if it is empty. This
# function is the logical complement of `assert_file_empty'.
#
# Globals:
#   BATSLIB_FILE_PATH_REM
#   BATSLIB_FILE_PATH_ADD
# Arguments:
#   $1 - path
# Returns:
#   0 - file is not empty
#   1 - otherwise
# Outputs:
#   STDERR - details, on failure
assert_file_not_empty() {
  local -r file="$1"
  if [[ ! -s "$file" ]]; then
    local -r rem="${BATSLIB_FILE_PATH_REM-}"
    local -r add="${BATSLIB_FILE_PATH_ADD-}"
    batslib_print_kv_single 4 'path' "${file/$rem/$add}" \
      | batslib_decorate 'file empty, but it was expected to contain something' \
      | fail
  fi
}

# Aliases to old assertion names
assert_exist() { assert_exists "$@"; }
assert_not_exist() { assert_not_exists "$@"; }
assert_file_exist() { assert_file_exists "$@"; }
assert_file_not_exist() { assert_file_not_exists "$@"; }
assert_dir_exist() { assert_dir_exists "$@"; }
assert_dir_not_exist() { assert_dir_not_exists "$@"; }
assert_link_exist() { assert_link_exists "$@"; }
assert_link_not_exist() { assert_link_not_exists "$@"; }
assert_block_exist() { assert_block_exists "$@"; }
assert_block_not_exist() { assert_block_not_exists "$@"; }
assert_character_exist() { assert_character_exists "$@"; }
assert_character_not_exist() { assert_character_not_exists "$@"; }
assert_socket_exist() { assert_socket_exists "$@"; }
assert_socket_not_exist() { assert_socket_not_exists "$@"; }
assert_fifo_exist() { assert_fifo_exists "$@"; }
assert_fifo_not_exist() { assert_fifo_not_exists "$@"; }
