#!/usr/bin/env bash
# CI: validate the POSIX self-heal launcher (npm/nub/bin/launch.js) against both
# package-manager bin-shim shapes, using a fake native (the heal is binary-agnostic
# — it only rewrites the on-PATH entry and exec's bin/<verb>). Asserts: first call
# runs, the PATH entry is healed into a #!/bin/sh trampoline -> native, the second
# call spawns ZERO node, nubx dispatches its own verb, and a foreign `nub` on PATH
# is never clobbered. Runs on ubuntu (dash) and macos (bash) — same launcher JS.
set -u
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
NUBPKG="$REPO/npm/nub"
W="$(mktemp -d)"; trap 'rm -rf "$W"' EXIT
PASS=0; FAIL=0
ok(){ echo "  ok: $1"; PASS=$((PASS+1)); }
no(){ echo "  FAIL: $1"; FAIL=$((FAIL+1)); }

# Fake native: prints a version, reports its verb from argv[0]'s basename (exactly
# how the real Rust CLI's Argv0::detect keys nub vs nubx).
FAKE="$W/fake-native"
cat > "$FAKE" <<'F'
#!/bin/sh
case "${0##*/}" in nubx*) echo "nubx-mode $*";; *) echo "nub 9.9.9-ci $*";; esac
F
chmod +x "$FAKE"

build(){ # $1 = pnpm | symlink
  rm -rf "$W/p"; mkdir -p "$W/p/node_modules/@nubjs/nub/bin" "$W/p/node_modules/@nubjs/nub-host/bin" "$W/p/node_modules/.bin" "$W/p/fakenode"
  cp "$NUBPKG/bin/nub" "$NUBPKG/bin/nubx" "$NUBPKG/bin/launch.js" "$W/p/node_modules/@nubjs/nub/bin/"
  cp "$NUBPKG/platform.js" "$W/p/node_modules/@nubjs/nub/"
  # main package.json: point the single optionalDependency at our fake host package
  node -e 'const fs=require("fs");const j=require(process.argv[1]+"/package.json");j.optionalDependencies={"@nubjs/nub-host":j.version};delete j.scripts;fs.writeFileSync(process.argv[2]+"/package.json",JSON.stringify(j))' "$NUBPKG" "$W/p/node_modules/@nubjs/nub"
  chmod +x "$W/p/node_modules/@nubjs/nub/bin/nub" "$W/p/node_modules/@nubjs/nub/bin/nubx"
  cp "$FAKE" "$W/p/node_modules/@nubjs/nub-host/bin/nub"
  cp "$FAKE" "$W/p/node_modules/@nubjs/nub-host/bin/nubx"
  chmod +x "$W/p/node_modules/@nubjs/nub-host/bin/"*
  printf '{"name":"@nubjs/nub-host","version":"9.9.9","files":["bin"]}\n' > "$W/p/node_modules/@nubjs/nub-host/package.json"
  # platform.js maps the host -> @nubjs/nub-host so resolveBinary finds the fake.
  printf 'module.exports={platformPackage(){return{key:"host",pkg:"@nubjs/nub-host"};}};\n' > "$W/p/node_modules/@nubjs/nub/platform.js"
  for v in nub nubx; do
    if [ "$1" = symlink ]; then ln -s "../@nubjs/nub/bin/$v" "$W/p/node_modules/.bin/$v"
    else cat > "$W/p/node_modules/.bin/$v" <<EOF
#!/bin/sh
basedir=\$(dirname "\$(echo "\$0" | sed -e 's,\\\\,/,g')")
exec node  "\$basedir/../@nubjs/nub/bin/$v" "\$@"
EOF
      chmod +x "$W/p/node_modules/.bin/$v"; fi
  done
  printf '#!/bin/sh\necho spawned >> "%s/p/node.log"\nexec %s "$@"\n' "$W" "$(command -v node)" > "$W/p/fakenode/node"; chmod +x "$W/p/fakenode/node"
}
BIN="$W/p/node_modules/.bin"
R(){ PATH="$BIN:$W/p/fakenode:$PATH" "$@" 2>&1; }

for STYLE in pnpm symlink; do
  echo "== $STYLE shim =="
  build "$STYLE"
  : > "$W/p/node.log"
  o=$(R "$BIN/nub" --version); case "$o" in *9.9.9-ci*) ok "nub first-call runs";; *) no "nub first-call: $o";; esac
  case "$(cat "$BIN/nub")" in "#!/bin/sh"*nub-host/bin/nub\'*) ok "nub healed -> sh trampoline";; *) no "nub not healed: $(tail -1 "$BIN/nub")";; esac
  : > "$W/p/node.log"; o2=$(R "$BIN/nub" --version); case "$o2" in *9.9.9-ci*) ok "nub second-call runs";; *) no "nub second: $o2";; esac
  [ -s "$W/p/node.log" ] && no "node spawned post-heal" || ok "zero node post-heal"
  : > "$W/p/node.log"; ox=$(R "$BIN/nubx" foo); case "$ox" in *nubx-mode*) ok "nubx verb dispatch";; *) no "nubx verb: $ox";; esac
  case "$(cat "$BIN/nubx")" in *nub-host/bin/nubx\'*) ok "nubx healed -> bin/nubx";; *) no "nubx not healed";; esac
done

echo "== verify-before-clobber =="
build pnpm
mkdir -p "$W/p/foreign"; printf '#!/bin/sh\necho foreign\n' > "$W/p/foreign/nub"; chmod +x "$W/p/foreign/nub"
b=$(cat "$W/p/foreign/nub")
PATH="$W/p/foreign:$BIN:$PATH" "$W/p/node_modules/@nubjs/nub/bin/nub" --version >/dev/null 2>&1
[ "$b" = "$(cat "$W/p/foreign/nub")" ] && ok "foreign nub untouched" || no "foreign CLOBBERED"

echo "RESULT: $PASS ok, $FAIL failed"
[ "$FAIL" -eq 0 ]
