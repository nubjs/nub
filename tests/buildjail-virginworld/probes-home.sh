#!/usr/bin/env bash
# Focused redo of P3's credential scan. The original matched "*.pem", which mostly hits the world's
# OWN ca-certificates bundle — noise, not a finding. The claim worth making is narrower and checkable:
# every credential-shaped path that EXISTS on this host is ABSENT inside, and $HOME is empty.
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WORLD="${WORLD:?}"; RUN="${RUN:-/tmp/vw-run}"; JAIL="$HERE/jail"
J=("$JAIL" --root "$WORLD" --bind "$RUN/work:/work" --uid 1000 --cwd /work)

echo "################ H1  the host's real credential set ################"
echo "\$ ls -A $HOME | wc -l"; ls -A "$HOME" | wc -l
for p in "$HOME/.ssh" "$HOME/.aws" "$HOME/.npmrc" "$HOME/.config" "$HOME/.docker" /root /etc/shadow; do
  [ -e "$p" ] && echo "HOST HAS   $p" || echo "host lacks $p"
done
echo "\$ ls -A $HOME/.ssh"; ls -A "$HOME/.ssh" 2>/dev/null | tr '\n' ' '; echo

echo
echo "################ H2  the same paths, from inside the world ################"
"${J[@]}" -- /bin/sh -c '
echo "HOME=$HOME   entries=$(ls -A "$HOME" | wc -l)"
echo "/home contains: $(ls -A /home | tr "\n" " ")"
for p in "$1/.ssh" "$1/.aws" "$1/.npmrc" "$1/.config" "$1/.docker" "$1"; do
  [ -e "$p" ] && echo "PRESENT $p" || echo "ABSENT  $p"
done
echo "--- /root inside is the WORLD/s own, and it is empty ---"
echo "/root entries: $(ls -A /root | wc -l)"
echo "--- /etc/shadow: the world has its own, with no real hashes ---"
[ -e /etc/shadow ] && head -2 /etc/shadow || echo "no /etc/shadow"
echo "--- exhaustive: every regular file under / that is NOT part of the base image or Node ---"
find / -xdev -type f \( -name "id_*" -o -name ".npmrc" -o -name ".netrc" -o -name "credentials" -o -name "*.kdbx" \) 2>/dev/null | wc -l
echo "(count of credential-shaped files in the entire world)"
' _ "$HOME"
echo "[rc=$?]"
