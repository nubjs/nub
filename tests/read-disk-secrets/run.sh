#!/usr/bin/env bash
# What does `read:"disk"` actually withhold? A four-probe differential against the real jail.
#
# ⛔ WHY THIS IS A SHELL HARNESS AND NOT A RUST ENFORCEMENT TEST. Reaching the `read:"disk"` rung
# needs a CATALOG GRANT, and there is no other way in: the shipped v1 model has no read-only-disk
# tier at all (`full_disk` is read AND write), so no compiled-in entry can express it. The only
# route is `NUB_BUILD_JAIL_CATALOG`, which lives behind the dev-only
# `nub-cli/build-jail-catalog-override` feature — and CI does not build with that feature today.
# A `tests/*.rs` case would therefore either not compile the seam or not run in CI. This harness
# builds the feature itself, so it works from a clean checkout with no CI change.
#
# WHAT IT ESTABLISHES, and every line of it is load-bearing:
#
#   probe                       no grant   read:"disk"   what it proves
#   ------------------------    --------   -----------   -------------------------------------
#   .env at an absolute path      DENIED     READ_OK     ⛔ THE FINDING
#   plain file, same directory    DENIED     READ_OK     the relaxation ENGAGED (positive control)
#   $HOME subtree secret          DENIED     DENIED      the allow-complement WORKS (bounds it)
#
# ⛔ THE SUBTREE CONTROL IS THE ONE PEOPLE WILL BE TEMPTED TO DROP, AND IT IS THE ONE THAT MAKES
# THIS A FINDING RATHER THAN AN ALARM. Without it, ".env is readable" is equally consistent with
# "`read:"disk"` hands over the whole filesystem" — a far worse claim. With it, the exposure is
# bounded to exactly the class `compiler/preset.rs` says cannot be expressed as an allow-complement:
# a depth-independent basename. `$HOME`-anchored SUBTREES (.ssh, .aws, .gnupg, Keychains, browser
# profiles) stay withheld.
#
# ⛔ TWO CONFOUNDS THIS HARNESS EXISTS TO AVOID, BOTH OF WHICH HAVE ALREADY PRODUCED WRONG READINGS:
#
#   1. A FIXTURE UNDER /tmp CANNOT TEST A DENIAL AT ALL. The jail redirects TMPDIR into a private
#      per-package directory, so a "secret" placed there is inside the grant by construction and
#      reads as allowed no matter what the policy says. Fixtures here live under $HOME.
#   2. A REUSED PACKAGE NAME REPLAYS A PRIOR RUN'S SCRIPT. `~/.cache/nub/jail-home/` accumulates an
#      entry per package name; a repeated name can surface a DIFFERENT script's output entirely.
#      Measured: a first attempt at this differential used a fixed name and got output from an
#      unrelated earlier experiment. Had it only grepped for the ABSENCE of a leak marker, both
#      arms would have read "denied" and the conclusion would have been the opposite of the truth.
#      ⇒ Every arm below uses a UNIQUE name AND prints a marker proving the intended script ran.
#      A missing marker is a hard failure, never a quiet pass.
#
# usage: run.sh [path-to-nub]     (builds the override-enabled binary if not given)
set -u

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
NUB=${1:-}
if [ -z "$NUB" ]; then
  echo "building nub with the catalog-override feature…"
  "$REPO/scripts/rust-build.sh" build -p nub-cli --profile fast \
    --features nub-cli/build-jail-catalog-override > /tmp/rds-build.log 2>&1
  rc=$?
  if [ "$rc" -ne 0 ]; then echo "BUILD FAILED (see /tmp/rds-build.log)"; exit 2; fi
  NUB="$("$REPO/scripts/rust-build.sh" --print-target 2>/dev/null | tail -1)/fast/nub"
fi
[ -x "$NUB" ] || { echo "no nub binary at $NUB"; exit 2; }
echo "nub: $NUB"

# A real $HOME-anchored subtree secret to probe. Never created — only an EXISTING one is used, and
# only its readability is tested (contents go to /dev/null and are never printed or logged).
SUBTREE=$(find "$HOME/Library/Keychains" "$HOME/.ssh" "$HOME/.aws" "$HOME/.gnupg" \
            -maxdepth 2 -type f 2>/dev/null | head -1)
if [ -z "$SUBTREE" ]; then
  echo "⚠ no existing \$HOME subtree secret found — the bounding control cannot run."
  echo "  Refusing to fabricate one in a real home. The .env result below would be UNBOUNDED:"
  echo "  it could not distinguish a .env-only leak from a whole-filesystem one."
fi

fail=0
arm() { # arm <label> <grant-json-or-empty>
  local label=$1 grant=$2
  local u="rds$(date +%s)$RANDOM"
  local r="$HOME/.nub-rds-$u"
  rm -rf "$r"; mkdir -p "$r/secret" "$r/$u" "$r/proj"
  printf 'SECRET-ENV=value\n' > "$r/secret/.env"
  printf 'PLAIN\n'            > "$r/secret/plainfile"
  cat > "$r/$u/package.json" <<JSON
{ "name": "$u", "version": "1.0.0", "scripts": { "postinstall": "sh ./pi.sh" } }
JSON
  {
    echo "echo MARKER-$u-RAN"
    echo "p(){ if cat \"\$1\" >/dev/null 2>&1; then echo \"\$2 READ_OK\"; else echo \"\$2 DENIED\"; fi; }"
    echo "p '$r/secret/.env' ENVFILE"
    echo "p '$r/secret/plainfile' PLAIN"
    [ -n "$SUBTREE" ] && echo "p '$SUBTREE' SUBTREE"
    echo "exit 0"
  } > "$r/$u/pi.sh"
  cat > "$r/proj/package.json" <<JSON
{ "name": "rdsproj", "version": "1.0.0", "private": true, "dependencies": { "$u": "file:../$u" } }
JSON

  # ⛔ NO ARRAY HERE, AND THAT IS DELIBERATE. Stock macOS `/bin/bash` is 3.2, where expanding an
  # EMPTY array as `"${arr[@]}"` under `set -u` aborts with `unbound variable`. The baseline arm
  # passes no env, so an array would kill exactly the arm that establishes the jail confines at
  # all — and only on machines without a newer bash on PATH, which is the worst kind of latent
  # break. Setting the variable in a subshell needs no array and works on every bash.
  if [ -n "$grant" ]; then
    printf '{ "packages": { "%s": { "default": %s } } }\n' "$u" "$grant" > "$r/cat.json"
    ( cd "$r/proj" && NUB_BUILD_JAIL_CATALOG="$r/cat.json" "$NUB" install )              > "$r/i.log" 2>&1
    ( cd "$r/proj" && NUB_BUILD_JAIL_CATALOG="$r/cat.json" "$NUB" approve-builds --all ) > "$r/a.log" 2>&1
  else
    ( cd "$r/proj" && "$NUB" install )              > "$r/i.log" 2>&1
    ( cd "$r/proj" && "$NUB" approve-builds --all ) > "$r/a.log" 2>&1
  fi

  echo "── $label"
  if ! grep -qh "MARKER-$u-RAN" "$r/i.log" "$r/a.log"; then
    echo "   ⛔ HARNESS ERROR: the postinstall never ran. NOT a verdict — do not read the probes."
    fail=1; rm -rf "$r"; return
  fi
  if [ -n "$grant" ]; then
    local ovr rej
    ovr=$(cat "$r"/*.log | grep -c 'catalog OVERRIDDEN'); rej=$(cat "$r"/*.log | grep -c 'REJECTED')
    echo "   override: OVERRIDDEN=$ovr REJECTED=$rej"
    if [ "$ovr" -lt 1 ] || [ "$rej" -ne 0 ]; then
      echo "   ⛔ HARNESS ERROR: the override did not engage — this arm measured the SHIPPED policy."
      fail=1; rm -rf "$r"; return
    fi
  fi
  grep -hE '^(ENVFILE|PLAIN|SUBTREE) ' "$r/i.log" "$r/a.log" | sed 's/^/   /'
  rm -rf "$r"
}

arm 'baseline — no catalog entry (everything must be DENIED)' ''
arm 'read:"disk" — .env leaks, subtree secret must NOT'       '{ "read": "disk" }'

echo
echo "EXPECTED, as of 2026-08-06: baseline denies all; read:\"disk\" gives ENVFILE READ_OK +"
echo "PLAIN READ_OK + SUBTREE DENIED. If SUBTREE ever reads OK, the allow-complement broke and"
echo "that is a far larger finding than the .env band. If ENVFILE ever reads DENIED, the"
echo "per-backend rendering step landed — update this harness and wiki/design/build-jail-*.md."
exit $fail
