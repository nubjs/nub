#!/usr/bin/env bash
# Heavy real-app build matrix (NIGHTLY / dedicated — NOT per-PR). Scaffolds two real apps,
# installs deps with `nub install`, and runs their builds under `nub`, asserting they
# complete without the resolveSync/loadSync async-loader crash class. The Node version is
# whatever `node` is on PATH (the matrix selects it; nub augments it).
#
#   app 1 — Next.js + Tailwind v4 + Turbopack: the IN-THE-WILD manifestation of the
#           resolveSync crash (PR #98 / the nextjs-build-compat thread). On a Node-broken
#           version with an un-fixed nub, `next build` aborts evaluating app/globals.css
#           with `Error: The resolveSync() method is not implemented`.
#   app 2 — Vite SSR build: exercises Vite's transform + rollup + a Node-side SSR render
#           under nub across the matrix. Chosen as a small, dependency-light second
#           real-world toolchain (no dev server / express needed — `vite build --ssr` +
#           executing the SSR bundle is fully reproducible in CI). Pinned to vite 6.
#
# Usage:  real-apps.sh <path-to-nub-binary> [--require-pass]
#   --require-pass : treat the Next/Tailwind resolveSync crash as a hard FAIL (use on
#                    Node-fixed legs and as the post-fix gate). Default: XFAIL on a crash
#                    (honest about the unfixed Node-broken bands) but FAIL on any OTHER
#                    build error.
set -uo pipefail

NUB="${1:?usage: real-apps.sh <nub-binary> [--require-pass]}"
REQUIRE_PASS=0
[[ "${2:-}" == "--require-pass" ]] && REQUIRE_PASS=1
NODE_VER="$(node --version 2>/dev/null || echo unknown)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/nub-realapps.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
echo "== real-apps on Node $NODE_VER ==  nub: $NUB  work: $WORK"

fails=0
crash_re='resolveSync|loadSync|ERR_METHOD_NOT_IMPLEMENTED|Error evaluating Node.js code'

# ENGINES-REDIRECT GUARD (defeats false-green from running the WRONG Node). nub discovers
# its Node from PATH, but an `engines.node` / `.node-version` / `packageManager` constraint
# that the PATH-selected Node does NOT satisfy makes nub REJECT it and fall through to the
# highest installed Node — so a leg labeled "Node 22.16" would silently build on 26 and mask
# version-specific bugs (the exact false-positive class this matrix exists to prevent). The
# scaffolded apps below ship a PERMISSIVE `engines.node` (`>=18`) precisely so no lower-tier
# leg is redirected; this assertion is the backstop that fails the leg if a redirect happens
# anyway (a transitive dep, a future edit, a floor bump). Assert nub runs the matrix-selected
# version BEFORE building anything.
# Run the probe from $WORK (a bare mktemp dir under TMPDIR, no package.json in its parents)
# so the probe ITSELF isn't redirected by the repo root's own engines pin.
ACTUAL_VER="$(cd "$WORK" && "$NUB" --eval 'process.stdout.write(process.version)' 2>/dev/null || true)"
if [[ -n "$ACTUAL_VER" && -n "$NODE_VER" && "$NODE_VER" != "unknown" && "$ACTUAL_VER" != "$NODE_VER" ]]; then
  echo "  FAIL  version redirect — matrix selected $NODE_VER but nub ran on $ACTUAL_VER (engines/pin masking this leg)"
  echo "== real-apps on Node $NODE_VER: 1 failure(s) =="
  exit 1
fi

judge() { # judge <label> <exit> <logfile>
  local label="$1" rc="$2" log="$3"
  if [[ $rc -eq 0 ]]; then echo "  PASS  $label"; return; fi
  if grep -qE "$crash_re" "$log"; then
    if [[ $REQUIRE_PASS -eq 1 ]]; then
      echo "  FAIL  $label — resolveSync/loadSync crash (nub did NOT recover)"; fails=$((fails + 1))
    else
      echo "  XFAIL $label — resolveSync/loadSync crash EXPECTED on Node-broken bands until nub recovers"
    fi
  else
    echo "  FAIL  $label — build failed for a non-crash reason (exit=$rc); tail:"; tail -15 "$log" | sed 's/^/        /'
    fails=$((fails + 1))
  fi
}

# ── app 1: Next 16 + Tailwind v4 + Turbopack ──────────────────────────────────
A1="$WORK/next-tw"; mkdir -p "$A1/app"
cat > "$A1/package.json" <<'EOF'
{ "name": "nub-next-tw", "private": true, "scripts": { "build": "next build" },
  "engines": { "node": ">=18" },
  "dependencies": { "next": "16.1.1", "react": "19.2.0", "react-dom": "19.2.0",
    "tailwindcss": "4.1.17", "@tailwindcss/postcss": "4.1.17" } }
EOF
# This matrix tests runtime compatibility, not the supply-chain soak window. Floating
# transitive releases must not make the fixture time-dependent.
printf 'minimumReleaseAge=0\n' > "$A1/.npmrc"
printf "module.exports = { typescript: { ignoreBuildErrors: true }, eslint: { ignoreDuringBuilds: true } };\n" > "$A1/next.config.js"
printf "export default { plugins: { '@tailwindcss/postcss': {} } };\n" > "$A1/postcss.config.mjs"
printf '@import "tailwindcss";\n' > "$A1/app/globals.css"
printf "import './globals.css';\nexport default function RootLayout({ children }) { return (<html><body>{children}</body></html>); }\n" > "$A1/app/layout.jsx"
printf "export default function Page() { return <main className=\"p-4\">ok</main>; }\n" > "$A1/app/page.jsx"

echo "-- next: nub install"
if ! ( cd "$A1" && "$NUB" install ) >"$WORK/next-install.log" 2>&1; then
  echo "  FAIL  next install"; tail -10 "$WORK/next-install.log" | sed 's/^/        /'; fails=$((fails + 1))
else
  echo "-- next: nub run build"
  ( cd "$A1" && "$NUB" run build ) >"$WORK/next-build.log" 2>&1
  judge "Next 16 + Tailwind v4 + Turbopack build" $? "$WORK/next-build.log"
fi

# ── app 2: Vite SSR build ─────────────────────────────────────────────────────
A2="$WORK/vite-ssr"; mkdir -p "$A2/src"
cat > "$A2/package.json" <<'EOF'
{ "name": "nub-vite-ssr", "private": true, "type": "module",
  "engines": { "node": ">=18" },
  "scripts": { "build": "vite build --ssr src/entry-server.js --outDir dist-server" },
  "devDependencies": { "vite": "6.0.7" } }
EOF
printf 'minimumReleaseAge=0\n' > "$A2/.npmrc"
cat > "$A2/src/entry-server.js" <<'EOF'
export function render() { return `<h1>VITE_SSR_OK</h1>`; }
EOF
cat > "$A2/run-ssr.mjs" <<'EOF'
const { render } = await import("./dist-server/entry-server.js");
const html = render();
if (!html.includes("VITE_SSR_OK")) { console.error("SSR render wrong:", html); process.exit(1); }
console.log("VITE_SSR_RENDER_OK");
EOF

echo "-- vite: nub install"
if ! ( cd "$A2" && "$NUB" install ) >"$WORK/vite-install.log" 2>&1; then
  echo "  FAIL  vite install"; tail -10 "$WORK/vite-install.log" | sed 's/^/        /'; fails=$((fails + 1))
else
  echo "-- vite: nub run build (SSR)"
  ( cd "$A2" && "$NUB" run build ) >"$WORK/vite-build.log" 2>&1
  vrc=$?
  if [[ $vrc -ne 0 ]]; then judge "Vite SSR build" $vrc "$WORK/vite-build.log"; else
    echo "-- vite: execute SSR bundle under nub"
    ( cd "$A2" && "$NUB" run-ssr.mjs ) >"$WORK/vite-render.log" 2>&1
    rrc=$?
    if [[ $rrc -eq 0 ]] && grep -q VITE_SSR_RENDER_OK "$WORK/vite-render.log"; then
      echo "  PASS  Vite SSR build + render"
    else
      judge "Vite SSR render" $rrc "$WORK/vite-render.log"
    fi
  fi
fi

echo "== real-apps on Node $NODE_VER: $fails failure(s) =="
exit $((fails > 0 ? 1 : 0))
