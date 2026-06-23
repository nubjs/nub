# `assert_regex`
#
# This function is similar to `assert_equal` but uses pattern matching instead
# of equality, by wrapping `[[ value =~ pattern ]]`.
#
# Fail if the value (first parameter) does not match the pattern (second
# parameter).
#
# ```bash
# @test 'assert_regex()' {
#   assert_regex 'what' 'x$'
# }
# ```
#
# On failure, the value and the pattern are displayed.
#
# ```
# -- values does not match regular expression --
# value    : what
# pattern  : x$
# --
# ```
#
# If the value is longer than one line then it is displayed in *multi-line*
# format.
#
# An error is displayed if the specified extended regular expression is invalid.
#
# For description of the matching behavior, refer to the documentation of the
# `=~` operator in the
# [Bash manual]: https://www.gnu.org/software/bash/manual/html_node/Conditional-Constructs.html.
# Note that the `BASH_REMATCH` array is available immediately after the
# assertion succeeds but is fragile, i.e. prone to being overwritten as a side
# effect of other actions.
assert_regex() {
	local -r value="${1}"
	local -r pattern="${2}"

	if ! __check_is_valid_regex "$pattern" assert_regex; then
		return 1
	elif ! [[ "${value}" =~ ${pattern} ]]; then
		if shopt -p nocasematch &>/dev/null; then
			local case_sensitive=insensitive
		else
			local case_sensitive=sensitive
		fi
		batslib_print_kv_single_or_multi 8 \
			'value' "${value}" \
			'pattern'  "${pattern}" \
			'case' "${case_sensitive}" \
		| batslib_decorate 'value does not match regular expression' \
		| fail
	fi
}
