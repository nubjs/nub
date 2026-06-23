# `refute_regex`
#
# This function is similar to `refute_equal` but uses pattern matching instead
# of equality, by wrapping `! [[ value =~ pattern ]]`.
#
# Fail if the value (first parameter) matches the pattern (second parameter).
#
# ```bash
# @test 'refute_regex()' {
#   refute_regex 'WhatsApp' 'Threema'
# }
# ```
#
# On failure, the value, the pattern and the match are displayed.
#
# ```
# @test 'refute_regex()' {
#   refute_regex 'WhatsApp' 'What.'
# }
#
# -- value matches regular expression --
# value    : WhatsApp
# pattern  : What.
# match    : Whats
# case     : sensitive
# --
# ```
#
# If the value or pattern is longer than one line then it is displayed in
# *multi-line* format.
#
# An error is displayed if the specified extended regular expression is invalid.
#
# For description of the matching behavior, refer to the documentation of the
# `=~` operator in the
# [Bash manual]: https://www.gnu.org/software/bash/manual/html_node/Conditional-Constructs.html.
#
# Note that the `BASH_REMATCH` array is available immediately after the
# assertion fails but is fragile, i.e. prone to being overwritten as a side
# effect of other actions like calling `run`. Thus, it's good practice to avoid
# using `BASH_REMATCH` in conjunction with `refute_regex()`. The valuable
# information the array contains is the matching part of the value which is
# printed in the failing test log, as mentioned above.
refute_regex() {
	local -r value="${1}"
	local -r pattern="${2}"

	if ! __check_is_valid_regex "${pattern}" "${FUNCNAME[0]}"; then
		return 1
	elif [[ "${value}" =~ ${pattern} ]]; then
		if shopt -p nocasematch &>/dev/null; then
			local case_sensitive=insensitive
		else
			local case_sensitive=sensitive
		fi
		batslib_print_kv_single_or_multi 8 \
			'value' "${value}" \
			'pattern'  "${pattern}" \
			'match' "${BASH_REMATCH[0]}" \
			'case' "${case_sensitive}" \
		| batslib_decorate 'value matches regular expression' \
		| fail
	fi
}
