# assert_line
# ===========
#
# Summary: Fail if the expected line is not found in the output (default) or at a specific line number.
#
# Usage: assert_line [-n index] [-p | -e] [--] <expected>
#
# Options:
#   -n, --index <idx> Match the <idx>th line
#   -p, --partial     Match if `expected` is a substring of `$output` or line <idx>
#   -e, --regexp      Treat `expected` as an extended regular expression
#   <expected>        The expected line string, substring, or regular expression
#
# IO:
#   STDERR - details, on failure
#            error message, on error
# Globals:
#   output
#   lines
# Returns:
#   0 - if matching line found
#   1 - otherwise
#
# Similarly to `assert_output`, this function verifies that a command or function produces the expected output.
# (It is the logical complement of `refute_line`.)
# It checks that the expected line appears in the output (default) or at a specific line number.
# Matching can be literal (default), partial or regular expression.
#
# *__Warning:__
# Due to a [bug in Bats][bats-93], empty lines are discarded from `${lines[@]}`,
# causing line indices to change and preventing testing for empty lines.*
#
# [bats-93]: https://github.com/sstephenson/bats/pull/93
#
# ## Looking for a line in the output
#
# By default, the entire output is searched for the expected line.
# The assertion fails if the expected line is not found in `${lines[@]}`.
#
#   ```bash
#   @test 'assert_line() looking for line' {
#     run echo $'have-0\nhave-1\nhave-2'
#     assert_line 'want'
#   }
#   ```
#
# On failure, the expected line and the output are displayed.
#
#   ```
#   -- output does not contain line --
#   line : want
#   output (3 lines):
#     have-0
#     have-1
#   have-2
#   --
#   ```
#
# ## Matching a specific line
#
# When the `--index <idx>` option is used (`-n <idx>` for short), the expected line is matched only against the line identified by the given index.
# The assertion fails if the expected line does not equal `${lines[<idx>]}`.
#
#   ```bash
#   @test 'assert_line() specific line' {
#     run echo $'have-0\nhave-1\nhave-2'
#     assert_line --index 1 'want-1'
#   }
#   ```
#
# On failure, the index and the compared lines are displayed.
#
#   ```
#   -- line differs --
#   index    : 1
#   expected : want-1
#   actual   : have-1
#   --
#   ```
#
# ## Partial matching
#
# Partial matching can be enabled with the `--partial` option (`-p` for short).
# When used, a match fails if the expected *substring* is not found in the matched line.
#
#   ```bash
#   @test 'assert_line() partial matching' {
#     run echo $'have 1\nhave 2\nhave 3'
#     assert_line --partial 'want'
#   }
#   ```
#
# On failure, the same details are displayed as for literal matching, except that the substring replaces the expected line.
#
#   ```
#   -- no output line contains substring --
#   substring : want
#   output (3 lines):
#     have 1
#     have 2
#     have 3
#   --
#   ```
#
# ## Regular expression matching
#
# Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
# When used, a match fails if the *extended regular expression* does not match the line being tested.
#
# *__Note__:
# As expected, the anchors `^` and `$` bind to the beginning and the end (respectively) of the matched line.*
#
#   ```bash
#   @test 'assert_line() regular expression matching' {
#     run echo $'have-0\nhave-1\nhave-2'
#     assert_line --index 1 --regexp '^want-[0-9]$'
#   }
#   ```
#
# On failure, the same details are displayed as for literal matching, except that the regular expression replaces the expected line.
#
#   ```
#   -- regular expression does not match line --
#   index  : 1
#   regexp : ^want-[0-9]$
#   line   : have-1
#   --
#   ```
# FIXME(ztombol): Display `${lines[@]}' instead of `$output'!
assert_line() {
  __assert_line "$@"
}

# assert_stderr_line
# ===========
#
# Summary: Fail if the expected line is not found in the stderr (default) or at a specific line number.
#
# Usage: assert_stderr_line [-n index] [-p | -e] [--] <expected>
#
# Options:
#   -n, --index <idx> Match the <idx>th line
#   -p, --partial     Match if `expected` is a substring of `$stderr` or line <idx>
#   -e, --regexp      Treat `expected` as an extended regular expression
#   <expected>        The expected line string, substring, or regular expression
#
# IO:
#   STDERR - details, on failure
#            error message, on error
# Globals:
#   stderr
#   stderr_lines
# Returns:
#   0 - if matching line found
#   1 - otherwise
#
# Similarly to `assert_stderr`, this function verifies that a command or function produces the expected stderr.
# (It is the logical complement of `refute_stderr_line`.)
# It checks that the expected line appears in the stderr (default) or at a specific line number.
# Matching can be literal (default), partial or regular expression.
#
assert_stderr_line() {
  __assert_line "$@"
}

__assert_line() {
  local -r caller=${FUNCNAME[1]}
  local -i is_match_line=0
  local -i is_mode_partial=0
  local -i is_mode_regexp=0

  if [[ "${caller}" == "assert_line" ]]; then
    : "${lines?}"
    local -ar stream_lines=("${lines[@]}")
    local -r stream_type=output
  elif [[ "${caller}" == "assert_stderr_line" ]]; then
    : "${stderr_lines?}"
    local -ar stream_lines=("${stderr_lines[@]}")
    local -r stream_type=stderr
  else
    # Unknown caller
    echo "Unexpected call to \`${FUNCNAME[0]}\`
Did you mean to call \`assert_line\` or \`assert_stderr_line\`?" \
    | batslib_decorate "ERROR: ${FUNCNAME[0]}" \
    | fail
    return $?
  fi

  # Handle options.
  while (( $# > 0 )); do
    case "$1" in
    -n|--index)
      if (( $# < 2 )) || ! [[ $2 =~ ^-?([0-9]|[1-9][0-9]+)$ ]]; then
        echo "\`--index' requires an integer argument: \`$2'" \
        | batslib_decorate "ERROR: ${caller}" \
        | fail
        return $?
      fi
      is_match_line=1
      local -ri idx="$2"
      shift 2
      ;;
    -p|--partial) is_mode_partial=1; shift ;;
    -e|--regexp) is_mode_regexp=1; shift ;;
    --) shift; break ;;
    *) break ;;
    esac
  done

  if (( is_mode_partial )) && (( is_mode_regexp )); then
    echo "\`--partial' and \`--regexp' are mutually exclusive" \
    | batslib_decorate "ERROR: ${caller}" \
    | fail
    return $?
  fi

  # Arguments.
  local -r expected="$1"

  if (( is_mode_regexp == 1 )); then
    __check_is_valid_regex "$expected" "$caller" || return 1
  fi

  # Matching.
  if (( is_match_line )); then
    # Specific line.
    if (( is_mode_regexp )); then
      if ! [[ ${stream_lines[$idx]} =~ $expected ]]; then
        batslib_print_kv_single 6 \
        'index' "$idx" \
        'regexp' "$expected" \
        'line'  "${stream_lines[$idx]}" \
        | batslib_decorate 'regular expression does not match line' \
        | fail
      fi
    elif (( is_mode_partial )); then
      if [[ ${stream_lines[$idx]} != *"$expected"* ]]; then
        batslib_print_kv_single 9 \
        'index'     "$idx" \
        'substring' "$expected" \
        'line'      "${stream_lines[$idx]}" \
        | batslib_decorate 'line does not contain substring' \
        | fail
      fi
    else
      if [[ ${stream_lines[$idx]} != "$expected" ]]; then
        batslib_print_kv_single 8 \
        'index'    "$idx" \
        'expected' "$expected" \
        'actual'   "${stream_lines[$idx]}" \
        | batslib_decorate 'line differs' \
        | fail
      fi
    fi
  else
    # Contained in output/error stream.
    if (( is_mode_regexp )); then
      local -i idx
      for (( idx = 0; idx < ${#stream_lines[@]}; ++idx )); do
        [[ ${stream_lines[$idx]} =~ $expected ]] && return 0
      done
      { local -ar single=( 'regexp' "$expected" )
        local -ar may_be_multi=( "${stream_type}" "${!stream_type}" )
        local -ir width="$( batslib_get_max_single_line_key_width "${single[@]}" "${may_be_multi[@]}" )"
        batslib_print_kv_single "$width" "${single[@]}"
        batslib_print_kv_single_or_multi "$width" "${may_be_multi[@]}"
      } \
      | batslib_decorate "no ${stream_type} line matches regular expression" \
      | fail
    elif (( is_mode_partial )); then
      local -i idx
      for (( idx = 0; idx < ${#stream_lines[@]}; ++idx )); do
        [[ ${stream_lines[$idx]} == *"$expected"* ]] && return 0
      done
      { local -ar single=( 'substring' "$expected" )
        local -ar may_be_multi=( "${stream_type}" "${!stream_type}" )
        local -ir width="$( batslib_get_max_single_line_key_width "${single[@]}" "${may_be_multi[@]}" )"
        batslib_print_kv_single "$width" "${single[@]}"
        batslib_print_kv_single_or_multi "$width" "${may_be_multi[@]}"
      } \
      | batslib_decorate "no ${stream_type} line contains substring" \
      | fail
    else
      local -i idx
      for (( idx = 0; idx < ${#stream_lines[@]}; ++idx )); do
        [[ ${stream_lines[$idx]} == "$expected" ]] && return 0
      done
      { local -ar single=( 'line' "$expected" )
        local -ar may_be_multi=( "${stream_type}" "${!stream_type}" )
        local -ir width="$( batslib_get_max_single_line_key_width "${single[@]}" "${may_be_multi[@]}" )"
        batslib_print_kv_single "$width" "${single[@]}"
        batslib_print_kv_single_or_multi "$width" "${may_be_multi[@]}"
      } \
      | batslib_decorate "${stream_type} does not contain line" \
      | fail
    fi
  fi
}
