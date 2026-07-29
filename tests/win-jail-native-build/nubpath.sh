#!/usr/bin/env bash
# The dev binary's path for this platform. A one-liner in its own file because the
# workflow's `shell: bash` runs with `-e`, where an inline `[ … ] && x=y` test whose
# condition is false aborts the whole step.
set -euo pipefail
if [ -x target/fast/nub.exe ]; then
  echo target/fast/nub.exe
else
  echo target/fast/nub
fi
