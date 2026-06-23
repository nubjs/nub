#
# bats-support - Supporting library for Bats test helpers
#
# Written in 2016 by Zoltan Tombol <zoltan dot tombol at gmail dot com>
#

#
# error.bash
# ----------
#
# Functions implementing error reporting. Used by public helper
# functions or test suits directly.
#

# Fail and display a message. When no parameters are specified, the
# message is read from the standard input. Other functions use this to
# report failure.
#
# Globals:
#   none
# Arguments:
#   $@ - [=STDIN] message
# Returns:
#   1 - always
# Inputs:
#   STDIN - [=$@] message
# Outputs:
#   STDERR - message
fail() {
  (( $# == 0 )) && batslib_err || batslib_err "$@"
  return 1
}
