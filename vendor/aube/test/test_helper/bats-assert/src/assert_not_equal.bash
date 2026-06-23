# assert_not_equal
# ============
#
# Summary: Fail if the actual and unexpected values are equal.
#
# Usage: assert_not_equal <actual> <unexpected>
#
# Options:
#   <actual>      The value being compared.
#   <unexpected>  The value to compare against.
#
#   ```bash
#   @test 'assert_not_equal()' {
#     assert_not_equal 'foo' 'foo'
#   }
#   ```
#
# IO:
#   STDERR - expected and actual values, on failure
# Globals:
#   none
# Returns:
#   0 - if actual does not equal unexpected
#   1 - otherwise
#
# On failure, the unexpected and actual values are displayed.
#
#   ```
#   -- values should not be equal --
#   unexpected : foo
#   actual     : foo
#   --
#   ```
assert_not_equal() {
  if [[ "$1" == "$2" ]]; then
    batslib_print_kv_single_or_multi 10 \
    'unexpected' "$2" \
    'actual'     "$1" \
    | batslib_decorate 'values should not be equal' \
    | fail
  fi
}
