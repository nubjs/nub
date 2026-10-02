#!/bin/sh
# GUI-launched Codex can inherit macOS's low launchd descriptor limit.
# Raise this hook process's soft limit before starting Node when the hard
# limit permits it; keep working under stricter environments otherwise.
ulimit -Sn 65536 2>/dev/null || true
exec "$@"
