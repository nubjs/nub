#!/bin/bash
# GCE startup script for a hand-made Linux VM: the box powers itself off once nothing has happened
# for 30 minutes. A guest poweroff reports as TERMINATED, so the disk survives and `start` resumes it.
#
# An open SSH connection is the signal that matters, and it is read from the socket table: `who`
# lists only sessions with a terminal, so it misses `ssh host command`. The stamp lives in /run so
# that a reboot starts a fresh idle window.
set -euo pipefail

install -m 0755 /dev/stdin /usr/local/sbin/nub-idle-stop <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
IDLE_MIN=30
STAMP=/run/nub-idle-since
busy() {
  [ -n "$(ss -Htn state established '( sport = :22 )')" ] && return 0
  pgrep -x 'cargo|rustc|nub' >/dev/null && return 0
  awk '{exit ($1 > 0.2) ? 0 : 1}' /proc/loadavg
}
if busy; then rm -f "$STAMP"; exit 0; fi
now=$(date +%s)
[ -f "$STAMP" ] || printf '%s' "$now" > "$STAMP"
if [ $(( (now - $(cat "$STAMP")) / 60 )) -ge "$IDLE_MIN" ]; then
  logger -t nub-idle "idle ${IDLE_MIN}m, powering off"
  systemctl poweroff
fi
SCRIPT

cat > /etc/systemd/system/nub-idle.service <<'UNIT'
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/nub-idle-stop
UNIT

cat > /etc/systemd/system/nub-idle.timer <<'UNIT'
[Timer]
OnBootSec=10min
OnUnitActiveSec=5min
[Install]
WantedBy=timers.target
UNIT

systemctl daemon-reload
systemctl enable --now nub-idle.timer
