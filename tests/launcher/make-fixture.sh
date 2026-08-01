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
# The on-PATH entry shape is chosen by $2:
#   symlink  npm / bun / yarn   -> symlink to ../node_modules/@nubjs/nub/bin/<verb>
#   pnpm     pnpm 10 cmd-shim   -> a #!/bin/sh shim that `exec node .../bin/<verb>`
#   pnpm11   pnpm >=11 cmd-shim -> same, plus empty `exe=""` assignments and a
#            trailing `# cmd-shim-target=` line (both parse hazards for leadsToUs)
#   pnpm11rel  as pnpm11, but the trailer is RELATIVE (pnpm resolves it against the
#            shim dir, not cwd — see leadsToUs)
#
# Usage: make-fixture.sh [dest] [symlink|pnpm|pnpm11|pnpm11rel]
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
case "${0##*/}" in
  nubx*) echo "nubx-mode $*";;
  *)     echo "nub 9.9.9-ci $*";;
esac
F
cp "$FAKE_SRC" "$HOSTPKG/bin/nub"
cp "$FAKE_SRC" "$HOSTPKG/bin/nubx"
rm -f "$FAKE_SRC"
# Land the fake native 0o644 (NO +x) — exactly how npm extracts a non-`bin`-field
# file. ensureExecutable() must recover this at runtime (chmod in place, or stage a
# copy when not owner). The heal/ensure code is what we're testing, so we do NOT
# pre-chmod it here.
chmod 0644 "$HOSTPKG/bin/nub" "$HOSTPKG/bin/nubx"
printf '{"name":"@nubjs/nub-host","version":"9.9.9","files":["bin"]}\n' \
  > "$HOSTPKG/package.json"

# The on-PATH entry (what dispatched us): symlink (npm/bun/yarn) or pnpm cmd-shim.
for v in nub nubx; do
  if [ "$STYLE" = symlink ]; then
    ln -s "../node_modules/@nubjs/nub/bin/$v" "$DEST/bin/$v"
  elif [ "$STYLE" = pnpm11 ] || [ "$STYLE" = pnpm11rel ]; then
    # pnpm >=11 cmd-shim. Two shape changes vs pnpm 10 that the heal must survive, both
    # copied from a real `pnpm@11.18.0 add` shim: the `exe=""`/`msys=""` EMPTY quote
    # pairs (which broke leadsToUs's quote scan — see launch.js), and the trailing
    # `# cmd-shim-target=` declaration. Keep both; either alone under-tests the parse.
    #
    # pnpm does NOT guarantee the trailer is absolute — its own reader path.resolve's it
    # against the shim's dir. pnpm11rel pins that shape; pnpm11 pins the absolute one.
    if [ "$STYLE" = pnpm11rel ]; then
      TARGET="../node_modules/@nubjs/nub/bin/$v"
    else
      TARGET="$LAUNCHER/bin/$v"
    fi
    cat > "$DEST/bin/$v" <<EOF
#!/bin/sh
basedir=\$(dirname "\$(echo "\$0" | sed -e 's,\\\\,/,g')")
basedir_win="\$basedir"
exe=""
msys=""

case \`uname -a\` in
  *CYGWIN*|*MINGW*|*MSYS*)
    exe=".exe"
    msys="true"
  ;;
esac

if [ -n "\$exe" ] && [ -x "\$basedir/node.exe" ]; then
  exec "\$basedir/node.exe"  "\$basedir_win/../node_modules/@nubjs/nub/bin/$v" "\$@"
else
  exec node  "\$basedir/../node_modules/@nubjs/nub/bin/$v" "\$@"
fi
# cmd-shim-target=$TARGET
EOF
    chmod +x "$DEST/bin/$v"
  else
    # pnpm 10 cmd-shim: a #!/bin/sh regular file that exec's node on the launcher.
    cat > "$DEST/bin/$v" <<EOF
#!/bin/sh
basedir=\$(dirname "\$(echo "\$0" | sed -e 's,\\\\,/,g')")
exec node  "\$basedir/../node_modules/@nubjs/nub/bin/$v" "\$@"
EOF
    chmod +x "$DEST/bin/$v"
  fi
done

# A `node` wrapper that LOGS every spawn to <dest>/node.log, so a test can assert the
# healed fast-path spawns ZERO node. Real node is found at fixture-build time.
REAL_NODE="$(command -v node)"
printf '#!/bin/sh\necho spawned >> "%s/node.log"\nexec %s "$@"\n' "$DEST" "$REAL_NODE" \
  > "$DEST/fakenode/node"
chmod +x "$DEST/fakenode/node"
: > "$DEST/node.log"

echo "fixture: $DEST (style=$STYLE)"
