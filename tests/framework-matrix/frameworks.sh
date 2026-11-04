#!/usr/bin/env bash
# frameworks.sh — the framework manifest for the GVS acceptance matrix. ONE place
# that maps a framework name to its OFFICIAL `create-*` scaffolder + the four
# commands `run.sh` drives (dev / build / preview / probe). Every fixture here is
# scaffolded by the framework's own real generator (`create-vite`, `create-astro`,
# `sv create`, `nuxi init`, `create-next-app`) — never hand-rolled — so the tree,
# the pinned framework/bundler versions, and the injected-dep shape are what a
# real user gets, not a synthetic approximation.
#
# Usage:  frameworks.sh scaffold <name> <dest>   # scaffolds into <dest>, prints DEV=/BUILD=/PREVIEW=/PROBE=/GVS=
#         frameworks.sh list                      # print the framework names
#
# Reproducibility: each scaffolder is pinned via a `*_CREATE` env var (below) —
# override to move a case to a different generator release. The scaffolder version
# fixes the framework/bundler major it emits; `run.sh` records the exact resolved
# versions at install time, so a drift is visible in the row, not silent.
#
# The five printed keys `run.sh`/`matrix.sh` consume (a value of `-` skips a step):
#   DEV=      dev-server command (binds 127.0.0.1:$PORT; run.sh parses the real port)
#   BUILD=    production build command
#   PREVIEW=  serve-the-build command (binds 127.0.0.1:$PPORT) — SSR/preview/start
#   PROBE=    a node_modules dep whose realpath headlines the layout verdict
#   GVS=      auto   → default install (respects nub's triggers; GVS engages)
#             exclude→ a realpath-locality-excluded framework (next/react-native):
#                      the trigger flips it PROJECT-LOCAL by design — never force GVS on
#             require→ GVS is forced ON for this fixture (its config makes the
#                      framework store-aware), and the verdict demands the store layout
set -u

# ── pinned scaffolder versions (override via env for a different major) ────────
# `npm create <x>` runs the `create-<x>` initializer, so VITE/ASTRO carry the
# bare `<x>@<ver>` npm passes to `create-`. sv/nuxi/next are `npx <pkg>` (full
# package name). Pin the major (e.g. VITE_CREATE=vite@7) to freeze the emitted
# framework/bundler version for reproducibility.
VITE_CREATE="${VITE_CREATE:-vite@latest}"       # → npm create vite@latest  (create-vite)
ASTRO_CREATE="${ASTRO_CREATE:-astro@latest}"     # → npm create astro@latest (create-astro)
SVELTE_CREATE="${SVELTE_CREATE:-sv@latest}"       # → npx sv create           (the SvelteKit CLI)
NUXT_CREATE="${NUXT_CREATE:-nuxi@latest}"         # → npx nuxi init
NEXT_CREATE="${NEXT_CREATE:-create-next-app@latest}" # → npx create-next-app
# The Next.js release the `next-gvs` arm pins (the generator's own pick lacks
# `experimental.turbopackAdditionalRoots`, which shipped in 16.4.0-canary.37).
# Bump to the first stable that carries it once 16.4.0 is out.
NEXT_GVS_VERSION="${NEXT_GVS_VERSION:-16.4.0-canary.37}"

# The framework names this manifest knows. Keep in sync with the case block below.
FRAMEWORKS=(vite-react vite-vue vite-svelte astro sveltekit nuxt next next-gvs)

_list() { printf '%s\n' "${FRAMEWORKS[@]}"; }

# create-vite: an official template. All create-vite@latest templates ship the
# current Vite (8.1+ today ⇒ native `.modules.yaml` allow-list path). $1=template.
# Runs from the parent dir with a bare project name — create-vite (and the other
# generators) treat the target as CWD-RELATIVE and strip a leading `/`.
_vite() {
  local base="$1" template="$2"
  npm create "$VITE_CREATE" "$base" -- --template "$template" >/dev/null 2>&1 || return 1
  echo "DEV=npx vite dev --port \$PORT --host 127.0.0.1"
  echo "BUILD=npx vite build"
  echo "PREVIEW=npx vite preview --port \$PPORT --host 127.0.0.1 --strictPort"
  echo "PROBE=vite"
  echo "GVS=auto"
}

scaffold() {
  local name="$1" dest="$2" parent base known
  # Validate the name against the known set BEFORE any destructive fs op. `dest`
  # is derived from a CLI arg; an unknown/garbage name (`..`, a stray path) must
  # never reach `rm -rf "$dest"` — `rm -rf "$OUT/.."` would wipe $OUT's parent.
  known=0; for f in "${FRAMEWORKS[@]}"; do [ "$f" = "$name" ] && known=1; done
  [ "$known" -eq 1 ] || { echo "UNKNOWN_FRAMEWORK: $name" >&2; return 2; }
  parent="$(dirname "$dest")"; base="$(basename "$dest")"
  rm -rf "$dest"; mkdir -p "$parent"
  # Every generator scaffolds into a CWD-relative <base>; cd to the parent first
  # so an absolute $dest lands at $dest (not $CWD/$dest with the slash stripped).
  cd "$parent" || return 1
  case "$name" in
    vite-react)  _vite "$base" react-ts ;;
    vite-vue)    _vite "$base" vue-ts ;;
    vite-svelte) _vite "$base" svelte-ts ;;

    # create-astro minimal: a static (SSG) Astro app. Astro embeds Vite
    # TRANSITIVELY (no top-level vite symlink) — the case that exercises the
    # phantom-eject ancestor-closure disk-materialization when its Vite is < 8.1.
    astro)
      npm create "$ASTRO_CREATE" "$base" -- --template minimal --install false --git false --skip-houston --yes >/dev/null 2>&1 || return 1
      echo "DEV=npx astro dev --port \$PORT --host 127.0.0.1"
      echo "BUILD=npx astro build"
      echo "PREVIEW=npx astro preview --port \$PPORT --host 127.0.0.1"
      echo "PROBE=astro"
      echo "GVS=auto" ;;

    # SvelteKit via the official `sv create` (successor to create-svelte).
    # adapter-auto → `vite preview` serves the built app.
    sveltekit)
      npx --yes "$SVELTE_CREATE" create "$base" --template minimal --types ts --no-add-ons >/dev/null 2>&1 || return 1
      echo "DEV=npx vite dev --port \$PORT --host 127.0.0.1"
      echo "BUILD=npx vite build"
      echo "PREVIEW=npx vite preview --port \$PPORT --host 127.0.0.1 --strictPort"
      echo "PROBE=@sveltejs/kit"
      echo "GVS=auto" ;;

    # Nuxt via `nuxi init`. SSR meta-framework: build emits `.output/`,
    # `nuxi preview` serves it (reads PORT from the env). Embeds Vite
    # transitively. KNOWN: Nuxt has a scule-phantom bug under GVS (fix in
    # flight) — a dev/prepare failure here is that known issue, not a new one.
    nuxt)
      npx --yes "$NUXT_CREATE" init "$base" --template minimal --no-install --packageManager npm --gitInit false >/dev/null 2>&1 || return 1
      echo "DEV=npx nuxi dev --port \$PORT --host 127.0.0.1"
      echo "BUILD=npx nuxi build"
      echo "PREVIEW=env PORT=\$PPORT npx nuxi preview"
      echo "PROBE=nuxt"
      echo "GVS=auto" ;;

    # Next via create-next-app (App Router, no turbopack for a stable build path).
    # next is REALPATH-LOCALITY-EXCLUDED from GVS (Turbopack chroots to one
    # project root) — the trigger installs it PROJECT-LOCAL. GVS=exclude tells
    # the matrix to expect (and accept) project-local, and to NEVER force GVS on.
    next)
      npx --yes "$NEXT_CREATE" "$base" --ts --no-eslint --no-tailwind --no-src-dir --app --no-turbopack --import-alias '@/*' --use-npm --skip-install >/dev/null 2>&1 || return 1
      echo "DEV=npx next dev --port \$PORT"
      echo "BUILD=npx next build"
      echo "PREVIEW=npx next start --port \$PPORT"
      echo "PROBE=next"
      echo "GVS=exclude" ;;

    # Next on the SHARED store: the same create-next-app scaffold (Turbopack on,
    # the 16.x default), pinned to a Next that has
    # `experimental.turbopackAdditionalRoots`, with nub's store configured as
    # the root. GVS=require forces the store on past nub's Next trigger and
    # demands the store layout in the verdict — this arm is what proves the
    # trigger can be dropped once Vercel ships the option. The root is computed
    # at `next` runtime from the same env the run uses (`--isolate-store`
    # relocates the store through XDG_CACHE_HOME after scaffolding), mirroring
    # nub's own cache-dir resolution. The release-age floor is lifted for the
    # fixture because a canary is younger than nub's 24-hour default.
    next-gvs)
      npx --yes "$NEXT_CREATE" "$base" --ts --no-eslint --no-tailwind --no-src-dir --app --turbopack --import-alias '@/*' --use-npm --skip-install >/dev/null 2>&1 || return 1
      node -e '
        const fs = require("fs");
        const p = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
        p.dependencies.next = process.argv[1];
        fs.writeFileSync(process.argv[2], JSON.stringify(p, null, 2) + "\n");
      ' "$NEXT_GVS_VERSION" "$base/package.json" || return 1
      printf 'minimumReleaseAge=0\n' > "$base/.npmrc"
      cat > "$base/next.config.ts" <<'CFG'
import os from "node:os";
import path from "node:path";
import type { NextConfig } from "next";

// nub's shared store, resolved the way nub resolves its cache dir.
const cacheDir =
  process.env.NUB_CACHE_DIR ??
  (process.env.XDG_CACHE_HOME
    ? path.join(process.env.XDG_CACHE_HOME, "nub")
    : process.platform === "win32" && process.env.LOCALAPPDATA
      ? path.join(process.env.LOCALAPPDATA, "nub")
      : path.join(os.homedir(), ".cache", "nub"));
const nubStore = path.join(cacheDir, "pm", "store");

const nextConfig: NextConfig = {
  experimental: {
    turbopackAdditionalRoots: { nubStore: { path: nubStore } },
  },
};

export default nextConfig;
CFG
      echo "DEV=npx next dev --port \$PORT"
      echo "BUILD=npx next build"
      echo "PREVIEW=npx next start --port \$PPORT"
      echo "PROBE=react"
      echo "GVS=require" ;;

    *) echo "UNKNOWN_FRAMEWORK: $name" >&2; return 2 ;;
  esac
}

case "${1:-}" in
  list)     _list ;;
  scaffold) scaffold "$2" "$3" ;;
  *) echo "usage: frameworks.sh {list | scaffold <name> <dest>}" >&2; exit 2 ;;
esac
