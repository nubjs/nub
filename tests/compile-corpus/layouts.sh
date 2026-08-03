#!/usr/bin/env bash
# Adversarial node_modules LAYOUTS, as opposed to the package corpus in run.sh.
#
# run.sh varies which package is compiled; this varies the SHAPE of the tree it
# sits in. That distinction matters because the payload path for an ejected
# package is derived from where it sits on disk, so a tree shape can produce a
# path collision that no ordinary package would.
#
# The nested case here found a real defect: two versions of one package at
# different depths both resolved to the same payload path, one silently replaced
# the other, and the artifact still exited 0 printing the right answer because
# those two versions happened to be compatible. Nothing observable from outside
# the binary would have caught it — which is why each case inspects the payload
# rather than only running the artifact.
#
# Usage: NUB=/path/to/nub tests/compile-corpus/layouts.sh [workdir]
set -uo pipefail

NUB="${NUB:?set NUB to the nub binary under test}"
WORK="${1:-${TMPDIR:-/tmp}/nub-compile-layouts}"
: "${NODE_PIN:=26.5.0}"
pass=0; fail=0

report() { # name result detail
  printf '%-22s %-6s %s\n' "$1" "$2" "${3:-}"
  if [ "$2" = PASS ]; then pass=$((pass+1)); else fail=$((fail+1)); fi
}

# Run an artifact with node_modules deleted, from an unrelated directory.
# Deleted rather than renamed: a rename leaves the tree reachable by a parent
# walk, so the artifact could pass by finding packages it never carried.
run_detached() { # dir binary -> echoes output
  mv "$1/node_modules" "$1/.nm" 2>/dev/null
  ( cd / && XDG_CACHE_HOME="$1/cache" "$2" 2>/dev/null | tail -1 )
  mv "$1/.nm" "$1/node_modules" 2>/dev/null
}

payload_dir() { ls -d "$1"/cache/nub/compile-app/*/ 2>/dev/null | head -1; }

printf '%-22s %-6s %s\n' LAYOUT RESULT DETAIL

# ---------------------------------------------------------------- nested dupes
# Two versions of one native package at different depths. npm dedupes, so the
# case has to be built by hand — which is also why it went unnoticed.
d="$WORK/nested"; rm -rf "$d"; mkdir -p "$d"; (
  cd "$d" && npm init -y >/dev/null 2>&1 && printf '%s\n' "$NODE_PIN" > .node-version
  npm i better-sqlite3 --no-audit --no-fund --silent
  mkdir -p node_modules/holder/node_modules
  cp -R node_modules/better-sqlite3 node_modules/holder/node_modules/better-sqlite3
  node -e 'const f="node_modules/holder/node_modules/better-sqlite3/package.json";
           const j=require("fs").readFileSync(f,"utf8");const o=JSON.parse(j);
           o.version="9.9.9-nested";require("fs").writeFileSync(f,JSON.stringify(o))'
  cat > node_modules/holder/package.json <<'EOF'
{"name":"holder","version":"1.0.0","main":"index.js","dependencies":{"better-sqlite3":"*"}}
EOF
  cat > node_modules/holder/index.js <<'EOF'
const D = require("better-sqlite3");
module.exports = () => { const db = new D(":memory:"); db.exec("create table h(x)"); return db.prepare("select count(*) c from h").get().c; };
EOF
  # A STATIC import. Using createRequire here would test the dynamic-require path
  # instead, and the resulting failure would be the fixture's, not nub's.
  cat > app.mjs <<'EOF'
import Database from "better-sqlite3";
import holder from "holder";
const db = new Database(":memory:"); db.exec("create table t(x)");
console.log("ok:" + db.prepare("select count(*) c from t").get().c + ":" + holder());
EOF
) >/dev/null 2>&1
if "$NUB" compile "$d/app.mjs" --out "$d/bin" >"$d/log" 2>&1; then
  out=$(run_detached "$d" "$d/bin")
  app=$(payload_dir "$d")
  n=$(find "$app/node_modules" -name package.json -path '*better-sqlite3*' 2>/dev/null | wc -l | tr -d ' ')
  if [ "$out" != "ok:0:0" ]; then report nested-versions FAIL "ran '$out', want ok:0:0"
  elif [ "$n" != 2 ]; then report nested-versions FAIL "$n copies in payload, want 2 — one overwrote the other"
  else report nested-versions PASS "both versions at distinct paths"; fi
else report nested-versions FAIL "compile failed"; fi

# --------------------------------------------------------------------- scoped
# A napi-rs package: JS-only wrapper plus a per-platform sidecar, both scoped.
# The scope directory must survive as a real directory.
d="$WORK/scoped"; rm -rf "$d"; mkdir -p "$d"; (
  cd "$d" && npm init -y >/dev/null 2>&1 && printf '%s\n' "$NODE_PIN" > .node-version
  npm i @node-rs/argon2 --no-audit --no-fund --silent
  cat > app.mjs <<'EOF'
import { hashSync } from "@node-rs/argon2";
console.log("ok:" + (hashSync("pw").startsWith("$argon2") ? 1 : 0));
EOF
) >/dev/null 2>&1
if "$NUB" compile "$d/app.mjs" --out "$d/bin" >"$d/log" 2>&1; then
  out=$(run_detached "$d" "$d/bin")
  app=$(payload_dir "$d")
  if [ "$out" != "ok:1" ]; then report scoped-napi FAIL "ran '$out', want ok:1"
  elif [ ! -d "$app/node_modules/@node-rs" ]; then report scoped-napi FAIL "scope directory flattened"
  else report scoped-napi PASS "scope preserved, wrapper + sidecar"; fi
else report scoped-napi FAIL "compile failed"; fi

# ------------------------------------------------------------------ workspace
# The dependency is reached through a symlinked workspace member.
d="$WORK/workspace"; rm -rf "$d"; mkdir -p "$d/packages/lib" "$d/packages/app"; (
  cd "$d" && printf '%s\n' "$NODE_PIN" > .node-version
  cat > package.json <<'EOF'
{"name":"root","private":true,"workspaces":["packages/*"]}
EOF
  cat > packages/lib/package.json <<'EOF'
{"name":"@ws/lib","version":"1.0.0","main":"index.js","dependencies":{"better-sqlite3":"*"}}
EOF
  cat > packages/lib/index.js <<'EOF'
const D = require("better-sqlite3");
module.exports = () => { const db = new D(":memory:"); db.exec("create table w(x)"); return db.prepare("select count(*) c from w").get().c; };
EOF
  cat > packages/app/package.json <<'EOF'
{"name":"@ws/app","version":"1.0.0","dependencies":{"@ws/lib":"1.0.0"}}
EOF
  cat > packages/app/app.mjs <<'EOF'
import lib from "@ws/lib";
console.log("ok:" + lib());
EOF
  npm install --no-audit --no-fund --silent
) >/dev/null 2>&1
if [ ! -L "$d/node_modules/@ws/lib" ]; then
  report workspace-symlink FAIL "npm did not symlink the member — fixture no longer tests this"
elif "$NUB" compile "$d/packages/app/app.mjs" --out "$d/bin" >"$d/log" 2>&1; then
  out=$(run_detached "$d" "$d/bin")
  if [ "$out" != "ok:0" ]; then report workspace-symlink FAIL "ran '$out', want ok:0"
  else report workspace-symlink PASS "resolved through the symlink"; fi
else report workspace-symlink FAIL "compile failed"; fi

# ------------------------------------------------------------------ data file
# A pure-JavaScript package that reads a data file relative to __dirname.
#
# Nothing in a manifest marks this package out: no native code, no resolver
# dependency, no platform fan. So it is bundled, __dirname collapses to the
# payload root, and the data file it wanted was never a module anyone imported.
# The artifact builds clean and fails at run time with ENOENT.
#
# --unbundled is the remedy, and this pins it: the package ships in its
# installed layout, its data file beside it, and __dirname points where its
# author expected. Both halves are asserted, because a test that only checked
# the fix would pass just as well if the problem had never existed.
d="$WORK/datafile"; rm -rf "$d"; mkdir -p "$d/node_modules/datapkg"; (
  cd "$d" && npm init -y >/dev/null 2>&1 && printf '%s\n' "$NODE_PIN" > .node-version
  cat > node_modules/datapkg/package.json <<'EOF'
{"name":"datapkg","version":"1.0.0","main":"index.js"}
EOF
  cat > node_modules/datapkg/index.js <<'EOF'
const fs = require("fs"), path = require("path");
module.exports = () => fs.readFileSync(path.join(__dirname, "table.txt"), "utf8").trim();
EOF
  printf 'PAYLOAD-OK\n' > node_modules/datapkg/table.txt
  cat > app.mjs <<'EOF'
import read from "datapkg";
console.log("ok:" + read());
EOF
) >/dev/null 2>&1
if ! "$NUB" compile "$d/app.mjs" --out "$d/plain" >"$d/log" 2>&1; then
  report datafile-eject FAIL "compile failed"
elif [ "$(run_detached "$d" "$d/plain")" = "ok:PAYLOAD-OK" ]; then
  report datafile-eject FAIL "bundled build worked — the case no longer reproduces"
elif ! "$NUB" compile "$d/app.mjs" --unbundled datapkg --out "$d/bin" >>"$d/log" 2>&1; then
  report datafile-eject FAIL "compile with --unbundled failed"
else
  rm -rf "$d/cache"
  out=$(run_detached "$d" "$d/bin")
  app=$(payload_dir "$d")
  if [ "$out" != "ok:PAYLOAD-OK" ]; then report datafile-eject FAIL "ran '$out', want ok:PAYLOAD-OK"
  elif [ ! -f "$app/node_modules/datapkg/table.txt" ]; then report datafile-eject FAIL "data file did not ship"
  else report datafile-eject PASS "--unbundled ships the data file and fixes __dirname"; fi
fi

# ------------------------------------------------------- isolated install tree
# Installed by nub itself rather than npm, which is the default a Nub user gets
# and a different SHAPE: `node_modules/<pkg>` is a symlink and the real package
# lives in `node_modules/.store/<pkg>@<version>/node_modules/<pkg>` with its own
# dependencies beside it, not hoisted to the top level.
#
# This found a defect nothing else could. Every other fixture here installs with
# npm, whose flat tree happens to put a transitive dependency exactly where a
# walk up from the symlink would look — so a walk that resolved from the wrong
# path passed everywhere and shipped a package with none of its dependencies.
# The binary compiled clean and died on `Cannot find module`.
d="$WORK/isolated"; rm -rf "$d"; mkdir -p "$d"; (
  cd "$d" && npm init -y >/dev/null 2>&1 && printf '%s\n' "$NODE_PIN" > .node-version
  cat > app.mjs <<'EOF'
import Database from "better-sqlite3";
const db = new Database(":memory:"); db.exec("create table t(x)");
console.log("ok:" + db.prepare("select count(*) c from t").get().c);
EOF
  "$NUB" add better-sqlite3
) >/dev/null 2>&1
if [ ! -L "$d/node_modules/better-sqlite3" ]; then
  report isolated-install FAIL "nub install did not symlink — fixture no longer tests this shape"
elif "$NUB" compile "$d/app.mjs" --out "$d/bin" >"$d/log" 2>&1; then
  out=$(run_detached "$d" "$d/bin")
  app=$(payload_dir "$d")
  deps=$(find "$app/node_modules" -name package.json -maxdepth 4 2>/dev/null | wc -l | tr -d ' ')
  if [ "$out" != "ok:0" ]; then report isolated-install FAIL "ran '$out', want ok:0"
  elif [ "$deps" -lt 2 ]; then report isolated-install FAIL "$deps packages in payload — dependencies did not ship"
  else report isolated-install PASS "symlinked tree, $deps packages shipped"; fi
else report isolated-install FAIL "compile failed"; fi

# ----------------------------------------------------------- peer dependency
# An ejected package that requires a PEER dependency at run time.
#
# A peer is normally supplied by the application rather than installed beneath
# the package, which makes it easy to read as somebody else's problem. It is not:
# an ejected package runs from real files and resolves its peer by walking up,
# exactly like any other require. Left out of the walk, the package shipped and
# the binary died on a module its own manifest named.
d="$WORK/peer"; rm -rf "$d"; mkdir -p "$d/node_modules/peerpkg" "$d/node_modules/thepeer"; (
  cd "$d" && npm init -y >/dev/null 2>&1 && printf '%s\n' "$NODE_PIN" > .node-version
  # node-gyp-build as a dependency is what classifies peerpkg as unbundlable,
  # which is the only way to reach the materialiser at all.
  mkdir -p node_modules/node-gyp-build
  cat > node_modules/node-gyp-build/package.json <<'EOF'
{"name":"node-gyp-build","version":"1.0.0","main":"index.js"}
EOF
  printf 'module.exports = () => ({});\n' > node_modules/node-gyp-build/index.js
  cat > node_modules/peerpkg/package.json <<'EOF'
{"name":"peerpkg","version":"1.0.0","main":"index.js","dependencies":{"node-gyp-build":"*"},"peerDependencies":{"thepeer":"*"}}
EOF
  printf 'module.exports = () => require("thepeer")();\n' > node_modules/peerpkg/index.js
  cat > node_modules/thepeer/package.json <<'EOF'
{"name":"thepeer","version":"1.0.0","main":"index.js"}
EOF
  printf 'module.exports = () => "PEER-OK";\n' > node_modules/thepeer/index.js
  cat > app.mjs <<'EOF'
import p from "peerpkg";
console.log("ok:" + p());
EOF
) >/dev/null 2>&1
if "$NUB" compile "$d/app.mjs" --out "$d/bin" >"$d/log" 2>&1; then
  out=$(run_detached "$d" "$d/bin")
  app=$(payload_dir "$d")
  if [ "$out" != "ok:PEER-OK" ]; then report peer-dependency FAIL "ran '$out', want ok:PEER-OK"
  elif [ ! -d "$app/node_modules/thepeer" ]; then report peer-dependency FAIL "the peer did not ship"
  else report peer-dependency PASS "peer shipped with its dependent"; fi
else report peer-dependency FAIL "compile failed"; fi

# --------------------------------------------------- cross-build with no target
# Cross-compiling against a tree that holds only the BUILD HOST's binaries must
# fail at build time. It is the one failure that is otherwise silent: the binary
# compiles clean and dies on the user's machine, which no output here would show.
#
# This has broken twice, both times as a side effect of a change that skipped
# work. The check keys on having SEEN an addon it could not use, so anything that
# stops walking a foreign package also stops the check from firing — most
# recently dropping foreign sidecars whole, which produced a clean 31 MB artifact
# that failed at run time.
#
# A napi-rs package is the right fixture and better-sqlite3 is not: the latter
# ships every platform's prebuild in one directory, so the target's binary is
# already present and there is nothing to refuse. npm installs only the host's
# sidecar for a napi-rs package, which is exactly the tree this guards.
#
# __NUB_LAUNCHER_TEMPLATE is cleared for this one case: the template is validated
# against the target BEFORE the addon check, so this host's launcher would fail
# on the template and never reach what is being tested. Nub then tries to fetch
# the target's launcher, and on an unreleased version there is nothing to fetch —
# so the case reports SKIP unless a launcher for the target is staged beside the
# nub binary (`nub-launcher-<platform>`). Reporting SKIP rather than PASS is the
# point: accepting the launcher error as "refused" would pass just as well with
# the addon check deleted, which is exactly the regression this guards.
d="$WORK/xbuild"; rm -rf "$d"; mkdir -p "$d"; (
  cd "$d" && npm init -y >/dev/null 2>&1 && printf '%s\n' "$NODE_PIN" > .node-version
  npm i @node-rs/argon2 --no-audit --no-fund --silent
  cat > app.mjs <<'EOF'
import { hashSync } from "@node-rs/argon2";
console.log("ok:" + (hashSync("pw").startsWith("$argon2") ? 1 : 0));
EOF
) >/dev/null 2>&1
# Prefer a foreign target whose launcher is already staged beside the nub binary,
# because that is the only way this case gets past the template check and
# actually exercises the guard. Falls back to a fixed target, which will SKIP.
foreign=""
for staged in "$(dirname "$NUB")"/nub-launcher-*; do
  [ -f "$staged" ] || continue
  cand="${staged##*/nub-launcher-}"
  cand="${cand%.exe}"
  case "$cand" in
    darwin-*) [ "$(uname -s)" = Darwin ] && continue ;;
    linux-*)  [ "$(uname -s)" = Linux ] && continue ;;
  esac
  foreign="$cand"; break
done
if [ -z "$foreign" ]; then
  case "$(uname -s)" in
    Darwin) foreign=linux-x64 ;;
    *)      foreign=darwin-arm64 ;;
  esac
fi
if (unset __NUB_LAUNCHER_TEMPLATE; "$NUB" compile "$d/app.mjs" --platform "$foreign" --out "$d/bin") >"$d/log" 2>&1; then
  report cross-build-guard FAIL "compiling for $foreign SUCCEEDED — the artifact would die at run time"
elif grep -qi "no nub-launcher template" "$d/log"; then
  printf '%-22s %-6s %s\n' cross-build-guard SKIP "no $foreign launcher to test against"
elif ! grep -qi "native addon is built for" "$d/log"; then
  report cross-build-guard FAIL "failed, but not with the platform diagnostic"
else
  report cross-build-guard PASS "refuses $foreign without the target's binaries"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" = 0 ]
