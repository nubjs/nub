#!/bin/sh
# GUI-launched Codex may inherit a low soft limit; respect the available hard limit.
soft=$(ulimit -Sn)
hard=$(ulimit -Hn)
limit=65536
case "$hard" in
  unlimited) ;;
  *) if [ "$hard" -lt "$limit" ]; then limit=$hard; fi ;;
esac
case "$soft" in
  unlimited) ;;
  *) if [ "$soft" -lt "$limit" ]; then ulimit -Sn "$limit" 2>/dev/null || true; fi ;;
esac
exec "$@"
