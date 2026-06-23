# assert_output
# =============
#
# Summary: Fail if `$output' does not match the expected output.
#
# Usage: assert_output [-p | -e] [- | [--] <expected>]
#
# Options:
#   -p, --partial  Match if `expected` is a substring of `$output`
#   -e, --regexp   Treat `expected` as an extended regular expression
#   -, --stdin     Read `expected` value from STDIN
#   <expected>     The expected value, substring or regular expression
#
# IO:
#   STDIN - [=$1] expected output
#   STDERR - details, on failure
#            error message, on error
# Globals:
#   output
# Returns:
#   0 - if output matches the expected value/partial/regexp
#   1 - otherwise
#
# This function verifies that a command or function produces the expected output.
# (It is the logical complement of `refute_output`.)
# Output matching can be literal (the default), partial or by regular expression.
# The expected output can be specified either by positional argument or read from STDIN by passing the `-`/`--stdin` flag.
#
# ## Literal matching
#
# By default, literal matching is performed.
# The assertion fails if `$output` does not equal the expected output.
#
#   ```bash
#   @test 'assert_output()' {
#     run echo 'have'
#     assert_output 'want'
#   }
#
#   @test 'assert_output() with pipe' {
#     run echo 'hello'
#     echo 'hello' | assert_output -
#   }
#
#   @test 'assert_output() with herestring' {
#     run echo 'hello'
#     assert_output - <<< hello
#   }
#   ```
#
# On failure, the expected and actual output are displayed.
#
#   ```
#   -- output differs --
#   expected : want
#   actual   : have
#   --
#   ```
#
# ## Existence
#
# To assert that any output exists at all, omit the `expected` argument.
#
#   ```bash
#   @test 'assert_output()' {
#     run echo 'have'
#     assert_output
#   }
#   ```
#
# On failure, an error message is displayed.
#
#   ```
#   -- no output --
#   expected non-empty output, but output was empty
#   --
#   ```
#
# ## Partial matching
#
# Partial matching can be enabled with the `--partial` option (`-p` for short).
# When used, the assertion fails if the expected _substring_ is not found in `$output`.
#
#   ```bash
#   @test 'assert_output() partial matching' {
#     run echo 'ERROR: no such file or directory'
#     assert_output --partial 'SUCCESS'
#   }
#   ```
#
# On failure, the substring and the output are displayed.
#
#   ```
#   -- output does not contain substring --
#   substring : SUCCESS
#   output    : ERROR: no such file or directory
#   --
#   ```
#
# ## Regular expression matching
#
# Regular expression matching can be enabled with the `--regexp` option (`-e` for short).
# When used, the assertion fails if the *extended regular expression* does not match `$output`.
#
# *__Note__:
# The anchors `^` and `$` bind to the beginning and the end (respectively) of the entire output;
# not individual lines.*
#
#   ```bash
#   @test 'assert_output() regular expression matching' {
#     run echo 'Foobar 0.1.0'
#     assert_output --regexp '^Foobar v[0-9]+\.[0-9]+\.[0-9]$'
#   }
#   ```
#
# On failure, the regular expression and the output are displayed.
#
#   ```
#   -- regular expression does not match output --
#   regexp : ^Foobar v[0-9]+\.[0-9]+\.[0-9]$
#   output : Foobar 0.1.0
#   --
#   ```
assert_output() {
  __assert_stream "$@"
}

# assert_stderr
# =============
#
# Summary: Fail if `$stderr' does not match the expected stderr.
#
# Usage: assert_stderr [-p | -e] [- | [--] <expected>]
#
# Options:
#   -p, --partial  Match if `expected` is a substring of `$stderr`
#   -e, --regexp   Treat `expected` as an extended regular expression
#   -, --stdin     Read `expected` value from STDIN
#   <expected>     The expected value, substring or regular expression
#
# IO:
#   STDIN - [=$1] expected stderr
#   STDERR - details, on failure
#            error message, on error
# Globals:
#   stderr
# Returns:
#   0 - if stderr matches the expected value/partial/regexp
#   1 - otherwise
#
# Similarly to `assert_output`, this function verifies that a command or function produces the expected stderr.
# (It is the logical complement of `refute_stderr`.)
# The stderr matching can be literal (the default), partial or by regular expression.
# The expected stderr can be specified either by positional argument or read from STDIN by passing the `-`/`--stdin` flag.
#
assert_stderr() {
  __assert_stream "$@"
}

__assert_stream() {
  local -r caller=${FUNCNAME[1]}
  local -r stream_type=${caller/assert_/}
  local -i is_mode_partial=0
  local -i is_mode_regexp=0
  local -i is_mode_nonempty=0
  local -i use_stdin=0

  if [[ ${stream_type} == "output" ]]; then
    : "${output?}"
  elif [[ ${stream_type} == "stderr" ]]; then
    : "${stderr?}"
  else
    # Unknown caller
    echo "Unexpected call to \`${FUNCNAME[0]}\`
Did you mean to call \`assert_output\` or \`assert_stderr\`?" |
      batslib_decorate "ERROR: ${FUNCNAME[0]}" |
      fail
    return $?
  fi
  local -r stream="${!stream_type}"

  # Handle options.
  if (( $# == 0 )); then
    is_mode_nonempty=1
  fi

  while (( $# > 0 )); do
    case "$1" in
    -p|--partial) is_mode_partial=1; shift ;;
    -e|--regexp) is_mode_regexp=1; shift ;;
    -|--stdin) use_stdin=1; shift ;;
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
  local expected
  if (( use_stdin )); then
    expected="$(cat -)"
  else
    expected="${1-}"
  fi

  # Matching.
  if (( is_mode_nonempty )); then
    if [ -z "$stream" ]; then
      echo "expected non-empty $stream_type, but $stream_type was empty" \
      | batslib_decorate "no $stream_type" \
      | fail
    fi
  elif (( is_mode_regexp )); then
    # shellcheck disable=2319
    if ! __check_is_valid_regex "$expected" "$caller"; then
      return 1
    elif ! [[ $stream =~ $expected ]]; then
      batslib_print_kv_single_or_multi 6 \
      'regexp'  "$expected" \
      "$stream_type" "$stream" \
      | batslib_decorate "regular expression does not match $stream_type" \
      | fail
    fi
  elif (( is_mode_partial )); then
    if [[ $stream != *"$expected"* ]]; then
      batslib_print_kv_single_or_multi 9 \
      'substring' "$expected" \
      "$stream_type"    "$stream" \
      | batslib_decorate "$stream_type does not contain substring" \
      | fail
    fi
  else
    if [[ $stream != "$expected" ]]; then
      batslib_print_kv_single_or_multi 8 \
      'expected' "$expected" \
      'actual'   "$stream" \
      | batslib_decorate "$stream_type differs" \
      | fail
    fi
  fi
}
