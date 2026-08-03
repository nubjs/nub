#!/usr/bin/env bash
# Build a reproducible "npm-global-style install" of @nubjs/nub into $1 (default
# /tmp/nub-launcher-fixture), wiring the cross-platform launcher (npm/nub/bin/*) to
# a FAKE native binary so the heal can be exercised without a real platform build
# (the heal is binary-agnostic — it only rewrites the on-PATH entry and exec's
# bin/<verb>). The fake native reports its verb from argv[0]'s basename, exactly how
# the real Rust CLI's Argv0::detect keys nub vs nubx.
#
# Layout produced (a real node_modules tree so launch.js's require.resolve finds the
# host package — exactly how `npm i -g` lays out @nubjs/nub + its platform package):
#   <dest>/node_modules/@nubjs/nub/{bin/{nub,nubx,launch.js},platform.js,package.json}
#   <dest>/node_modules/@nubjs/nub-host/bin/{nub,nubx}   <- the fake native, mode 0644
#   <dest>/bin/{nub,nubx}                                <- the on-PATH entry (style arg)
#   <dest>/fakenode/node                                 <- a node wrapper logging spawns
#
# The on-PATH entry shape is chosen by $2. The first three reproduce what a real PM
# writes; the last three are DERIVED shapes that each isolate ONE leadsToUs mechanism,
# because leadsToUs tries the `# cmd-shim-target=` trailer before the quote scan and
# returns early — so a fixture carrying both hazards (i.e. a verbatim pnpm 11 shim)
# cannot fail when either mechanism alone regresses. Verified by reverting each
# launcher change independently; see README.md.
#
#   symlink   npm / bun / yarn   -> symlink to ../node_modules/@nubjs/nub/bin/<verb>
#   pnpm      pnpm 10 cmd-shim   -> a #!/bin/sh shim that `exec node .../bin/<verb>`
#   pnpm11    pnpm >=11 cmd-shim -> verbatim @zkochan/cmd-shim 9.0.6 output: the 5-branch
#             exec chain, the *WSL2* arm, the cygpath call, the empty `exe=""`/`msys=""`
#             pairs, and the trailing `# cmd-shim-target=`. The real-world shape.
#   scan      pnpm11 minus the trailer -> only the quote scan can match it, so it goes
#             red if the `[^"]*` class regresses to `[^"]+` (the pnpm 11 field bug)
#   decl      target assembled UNQUOTED (`exec node $target`), absolute trailer -> no
#             quoted token resolves to us, so only the trailer branch can match it
#   declrel   as decl, with a RELATIVE trailer -> additionally goes red if the trailer
#             is not path.resolve'd against the shim dir (bare realpath uses cwd)
#
# Usage: make-fixture.sh [dest] [symlink|pnpm|pnpm11|scan|decl|declrel]
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
NUBPKG="$REPO_DIR/npm/nub"

DEST="${1:-/tmp/nub-launcher-fixture}"
STYLE="${2:-symlink}"

NM="$DEST/node_modules"
LAUNCHER="$NM/@nubjs/nub"
HOSTPKG="$NM/@nubjs/nub-host"

rm -rf "$DEST"
mkdir -p "$LAUNCHER/bin" "$HOSTPKG/bin" "$DEST/bin" "$DEST/fakenode"

# The cross-platform launcher package (what `npm i -g @nubjs/nub` extracts).
cp "$NUBPKG/bin/nub" "$NUBPKG/bin/nubx" "$NUBPKG/bin/launch.js" "$LAUNCHER/bin/"
chmod +x "$LAUNCHER/bin/nub" "$LAUNCHER/bin/nubx"
# platform.js is replaced with a stub that maps every platform -> @nubjs/nub-host,
# so resolveBinary() finds the fake native regardless of host arch/libc.
printf 'module.exports={platformPackage(){return{key:"host",pkg:"@nubjs/nub-host"};}};\n' \
  > "$LAUNCHER/platform.js"
# package.json: a single optionalDependency at the fake host package, scripts dropped
# (the harness exercises the RUNTIME heal, independent of postinstall).
node -e '
  const fs=require("fs");
  const j=require(process.argv[1]+"/package.json");
  j.optionalDependencies={"@nubjs/nub-host":j.version};
  delete j.scripts;
  fs.writeFileSync(process.argv[2]+"/package.json",JSON.stringify(j));
' "$NUBPKG" "$LAUNCHER"

# The FAKE native binary: echoes its verb (from argv0 basename) + args, like the real
# CLI's Argv0 dispatch. Two copies, one per verb, in the host package's bin.
FAKE_SRC="$DEST/.fake-native"
cat > "$FAKE_SRC" <<'F'
#!/bin/sh
verb="${__NUB_ARGV0:-${0##*/}}"
case "$verb" in
  nubx*) echo "nubx-mode $*";;
  *)     echo "nub 9.9.9-ci $*";;
esac
F
# ONE binary, matching the real platform package. The `__NUB_ARGV0`-before-argv[0]
# precedence above mirrors Argv0::detect / capture_argv0_override, and the order is
# load-bearing: on the healed fast path argv[0] is `nub` for BOTH verbs, so only the
# env var separates them. A fixture that consulted argv[0] first would go green while
# the real binary silently ran the wrong verb.
cp "$FAKE_SRC" "$HOSTPKG/bin/nub"
rm -f "$FAKE_SRC"
# Land the fake native 0o644 (NO +x) — exactly how npm extracts a non-`bin`-field
# file. ensureExecutable() must recover this at runtime (chmod in place, or stage a
# copy when not owner). The heal/ensure code is what we're testing, so we do NOT
# pre-chmod it here.
chmod 0644 "$HOSTPKG/bin/nub"
printf '{"name":"@nubjs/nub-host","version":"9.9.9","files":["bin"]}\n' \
  > "$HOSTPKG/package.json"

# Substitute into a shim template read on stdin. The templates below use QUOTED
# heredocs and these two placeholders so they stay byte-faithful to what the real
# generator emits — an unquoted heredoc would need every `$`, backtick and `$(...)`
# escaped, which is exactly how a fixture drifts from the shape it claims to pin.
emit() { sed -e "s|__VERB__|$1|g" -e "s|__TARGET__|$2|g"; }

# The on-PATH entry (what dispatched us): symlink (npm/bun/yarn) or a cmd-shim.
for v in nub nubx; do
  case "$STYLE" in
    symlink)
      ln -s "../node_modules/@nubjs/nub/bin/$v" "$DEST/bin/$v"
      continue
      ;;
    pnpm11|scan)
      # Verbatim @zkochan/cmd-shim 9.0.6 output — the generator pnpm >=11's bin-linker
      # resolves. Reproduced in full (5-branch exec chain, the *WSL2* arm, the cygpath
      # call) rather than simplified, so it drifts only when upstream's template does.
      # `exe=""`/`msys=""` are the empty quote pairs that broke leadsToUs's scan.
      emit "$v" "$LAUNCHER/bin/$v" > "$DEST/bin/$v" <<'EOF'
#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\,/,g')")
basedir_win="$basedir"
exe=""
msys=""

case `uname -a` in
  *CYGWIN*|*MINGW*|*MSYS*)
    if command -v cygpath > /dev/null 2>&1; then
      basedir_win=`cygpath -w "$basedir"`
    fi
    exe=".exe"
    msys="true"
  ;;
  *WSL2*)
    if command -v wslpath > /dev/null 2>&1; then
      basedir_win="$(wslpath -w "$basedir" 2> /dev/null)"
      if [ $? -ne 0 ] || [ -z "$basedir_win" ]; then
        basedir_win="$basedir"
      else
        exe=".exe"
      fi
    fi
  ;;
esac

if [ -n "$exe" ] && [ -x "$basedir/node.exe" ]; then
  exec "$basedir/node.exe"  "$basedir_win/../node_modules/@nubjs/nub/bin/__VERB__" "$@"
elif [ -x "$basedir/node" ]; then
  exec "$basedir/node"  "$basedir/../node_modules/@nubjs/nub/bin/__VERB__" "$@"
elif command -v node >/dev/null 2>&1; then
  exec node  "$basedir/../node_modules/@nubjs/nub/bin/__VERB__" "$@"
elif [ -n "$exe" ] && command -v node.exe >/dev/null 2>&1; then
  exec node.exe  "$basedir_win/../node_modules/@nubjs/nub/bin/__VERB__" "$@"
else
  exec node  "$basedir/../node_modules/@nubjs/nub/bin/__VERB__" "$@"
fi
EOF
      # pnpm11 is the real thing, trailer included. `scan` withholds the trailer so the
      # quote scan is the ONLY route to a match — that is what makes it able to fail.
      if [ "$STYLE" = pnpm11 ]; then
        printf '# cmd-shim-target=%s\n' "$LAUNCHER/bin/$v" >> "$DEST/bin/$v"
      fi
      ;;
    decl|declrel)
      # A shim whose target is assembled into an UNQUOTED variable, so no quoted token
      # resolves to us and the trailer is the only route to a match. Not a shape pnpm
      # emits — it exists to give the trailer branch a failure mode, which a verbatim
      # pnpm 11 shim cannot (leadsToUs returns on the trailer before scanning quotes).
      if [ "$STYLE" = declrel ]; then TARGET="../node_modules/@nubjs/nub/bin/$v"
      else TARGET="$LAUNCHER/bin/$v"; fi
      emit "$v" "$TARGET" > "$DEST/bin/$v" <<'EOF'
#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\,/,g')")
target=$basedir/../node_modules/@nubjs/nub/bin/__VERB__
exec node $target "$@"
# cmd-shim-target=__TARGET__
EOF
      ;;
    pnpm)
      # pnpm 10 cmd-shim: a #!/bin/sh regular file that exec's node on the launcher.
      # No empty quote pair and no trailer — which is why pnpm 10 never hit the bug.
      emit "$v" "" > "$DEST/bin/$v" <<'EOF'
#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\,/,g')")
exec node  "$basedir/../node_modules/@nubjs/nub/bin/__VERB__" "$@"
EOF
      ;;
    *)
      # Never fall through to a default shape. The matrix spells all six style names by
      # hand, so a misspelling there would otherwise build the pnpm 10 shim, pass, and
      # pin nothing — a green leg guarding a mechanism nobody is testing is the exact
      # failure this harness exists to prevent.
      echo "make-fixture.sh: unknown style '$STYLE'" >&2
      echo "  expected: symlink | pnpm | pnpm11 | scan | decl | declrel" >&2
      exit 2
      ;;
  esac
  chmod +x "$DEST/bin/$v"
done

# A `node` wrapper that LOGS every spawn to <dest>/node.log, so a test can assert the
# healed fast-path spawns ZERO node. Real node is found at fixture-build time.
REAL_NODE="$(command -v node)"
printf '#!/bin/sh\necho spawned >> "%s/node.log"\nexec %s "$@"\n' "$DEST" "$REAL_NODE" \
  > "$DEST/fakenode/node"
chmod +x "$DEST/fakenode/node"
: > "$DEST/node.log"

echo "fixture: $DEST (style=$STYLE)"
