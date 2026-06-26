#!/bin/sh
# Entrypoint mirroring oven/bun's: if the first arg looks like a flag, is not an
# executable on PATH, or is a non-executable file, treat the whole arg list as
# arguments to `nub` (so `docker run nub:slim script.ts` runs the file). Otherwise
# exec the command verbatim (so `docker run nub:slim sh` opens a shell).
set -e

if [ "${1#-}" != "${1}" ] || [ -z "$(command -v "${1}")" ] || { [ -f "${1}" ] && ! [ -x "${1}" ]; }; then
  set -- nub "$@"
fi

exec "$@"
