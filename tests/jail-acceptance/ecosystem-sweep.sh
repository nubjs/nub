#!/usr/bin/env bash
# Does a ~99.5% swath of the MODERN ecosystem install with the jail DEFAULT-ON?
#
# ⛔⛔ THIS IS THE MEASUREMENT THAT DECIDES SHIPPING, AND IT IS NOT THE ONE THE CORPUS MAKES. The corpus
# sweeps 6880 mostly-old, mostly-niche package-VERSIONS and probes each at the WIDEST grant
# (`{write:"disk", network:true}`) to answer "is the jail the cause of this failure?". That is a debugging
# question. The shipping question is different in both dimensions:
#
#   - GRANT: does it install at the grant nub actually SHIPS — the compiled catalog, no override?
#   - POPULATION: does it install for the packages real users actually pull, at the versions they pull
#     TODAY? A flat pass rate over historical package-versions says nothing about a userbase-weighted bar,
#     because one broken `@pulumi/gcp@0.16.9` and one broken `next` are not the same event.
#
# So the population here is REAL PROJECTS: the dependency sets a developer gets from the current
# scaffolding of each major framework and toolchain. If those install clean, default-on, on a cold machine,
# that IS the 99.5% swath — a package with an install script that no popular tree pulls cannot move a
# userbase-weighted number, and a niche package needing `no-jail` is an accepted outcome.
#
# ⛔ EVERY RUN IS COLD, AND THAT IS THE POINT. A warm download cache hides exactly the failure that matters:
# `electron` was measured as needing no network because the measurement machine already had its zip, and
# the resulting grant made a cold install fail `getaddrinfo ENOTFOUND github.com`. Each project gets a
# fresh HOME and a fresh nub cache.
#
# ⛔ THE JAIL IS NOT CONFIGURED HERE. No catalog override, no `no-jail`, no `dependenciesMeta`. Whatever the
# shipped binary does by default is what this measures — that is the entire value of the number.
#
# Usage: NUB=/path/to/nub ./ecosystem-sweep.sh [--only <name>] [--keep-logs <dir>]
set -uo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
NUB="${NUB:-$ROOT/target/fast/nub}"
ONLY=""; KEEP=""
while [ $# -gt 0 ]; do
  case "$1" in
    --only) ONLY="$2"; shift 2 ;;
    --keep-logs) KEEP="$2"; shift 2 ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
[ -x "$NUB" ] || { echo "no nub binary at $NUB" >&2; exit 2; }
OUT="${OUT:-/tmp/ecosystem-sweep.tsv}"
: > "$OUT"
[ -n "$KEEP" ] && mkdir -p "$KEEP"

# The dependency sets, as a developer gets them from current scaffolding. Kept as DEPENDENCY LISTS rather
# than by running each `create-*` tool: a scaffolder is interactive, network-heavy and version-drifting,
# and what is under test is the install of the tree it produces, not the scaffolder itself.
#
# `latest` throughout, deliberately — the bar is about the MODERN ecosystem, and pinning would measure a
# frozen past. It also means this sweep's answer can change when a dependency publishes, which is a
# property to want: that is the same exposure a user has.
read -r -d '' PROJECTS <<'ROWS' || true
next-app	next react react-dom typescript @types/node
vite-react	vite @vitejs/plugin-react react react-dom typescript
nuxt	nuxt vue
angular	@angular/core @angular/cli @angular/compiler-cli typescript zone.js rxjs
svelte-kit	@sveltejs/kit @sveltejs/adapter-auto svelte vite
astro	astro
remix	@remix-run/node @remix-run/react @remix-run/dev
nest	@nestjs/core @nestjs/common @nestjs/platform-express reflect-metadata rxjs
electron-app	electron electron-builder
electron-only	electron
image-pipeline	sharp imagemin
db-native	better-sqlite3 prisma
test-browsers	playwright puppeteer
bundlers	esbuild rollup webpack terser
tooling	eslint prettier typescript vitest
grpc-stack	@grpc/grpc-js protobufjs
node-native-misc	bcrypt node-gyp-build canvas
ROWS

printf '%s\n' "$PROJECTS" | while IFS=$'\t' read -r name deps; do
  [ -n "${name:-}" ] || continue
  [ -n "$ONLY" ] && [ "$ONLY" != "$name" ] && continue
  # ⛔ THE PROJECT LIVES INSIDE THE HOME whose cache holds the store. Sibling temp dirs made node-gyp's
  # relative path to a store dependency resolve nowhere and manufactured a `sharp` failure that does not
  # exist for users — caught only because the same package installed fine when the layout matched a real
  # machine's. A harness whose own directory layout changes the answer is measuring itself.
  home="$(mktemp -d "$HOME/eco-XXXXXX")"
  proj="$home/project"
  mkdir -p "$proj"
  # No allowBuilds: the DEFAULT approval behaviour is part of what is being measured.
  {
    printf '{"name":"eco-%s","version":"1.0.0","dependencies":{' "$name"
    first=1
    for d in $deps; do
      [ "$first" = 1 ] || printf ','
      printf '"%s":"latest"' "$d"
      first=0
    done
    printf '}}\n'
  } > "$proj/package.json"
  printf 'side-effects-cache=false\n' > "$proj/.npmrc"

  log="$home/install.log"
  ( cd "$proj" && env -u ELECTRON_CACHE -u ELECTRON_MIRROR -u PLAYWRIGHT_BROWSERS_PATH \
      -u PUPPETEER_CACHE_DIR -u npm_config_cache \
      HOME="$home" NUB_CACHE_DIR="$home/.cache/nub" timeout 1800 "$NUB" install > "$log" 2>&1 )
  rc=$?

  # ⛔ THE JAIL-ATTRIBUTION TEST READS THE LOG, NOT THE EXIT CODE, and it names the jail's own strings.
  # An exit code cannot say WHY, and a summary column cannot either — grepping a summary instead of the log
  # is how 6 win32 jail refusals were missed once already.
  jail_blocked=$(grep -ciE 'nub build sandbox: blocked|could not confine|sandbox could not be applied|WARN_NUB_JAIL_NET_DENIED' "$log" 2>/dev/null || true)
  # A package the jail SKIPPED for want of approval is a different outcome from one it broke: the script
  # never ran, which is nub's documented default for an unapproved build, not a jail failure.
  skipped=$(grep -ciE 'WARN_NUB_IGNORED_BUILD_SCRIPTS|ignored build scripts' "$log" 2>/dev/null || true)
  # ⛔⛔ HOW MANY LIFECYCLE SCRIPTS ACTUALLY RAN — WITHOUT THIS THE WHOLE SWEEP CAN BE VACUOUS. The first
  # run of this reported "12 OK, 0 jail-involved" and it was mostly meaningless: only 6 of 16 projects ran
  # ANY install script, so in ten of them the jail was never exercised and "OK" meant nothing about it.
  # `sharp` was among the zeros — the modern ecosystem has largely moved from install scripts to
  # platform-specific optionalDependencies, which is a real and welcome fact about the blast radius but is
  # NOT evidence that confinement works. A row with no script run is NO-EVIDENCE, never OK.
  ran=$(grep -ciE 'defaultTrust: running build scripts|running build scripts for' "$log" 2>/dev/null || true)

  # ⛔⛔ A DENIED EGRESS DOES NOT ALWAYS SAY "JAIL". On macOS Seatbelt refuses the socket and the package
  # sees a DNS failure, so the log reads `getaddrinfo ENOTFOUND github.com` with no jail-branded line
  # anywhere — MEASURED, by giving electron a catalog entry that grants nothing and watching exactly that.
  # A detector that only greps the jail's own strings therefore UNDER-REPORTS its involvement on the
  # platform most of this project's testing happens on, which would turn a real breakage into
  # FAILED-OTHER. Network errors get their own bucket rather than being folded into either side: they are
  # not proof of a denial (a registry can genuinely be unreachable), and they are not safe to dismiss.
  net_err=$(grep -ciE 'ENOTFOUND|EAI_AGAIN|ECONNREFUSED|ETIMEDOUT' "$log" 2>/dev/null || true)
  if [ "$rc" = 0 ] && [ "$jail_blocked" = 0 ] && [ "$ran" = 0 ]; then
    # Installed fine, but nothing was confined, so this row says nothing about the jail.
    verdict=NO-EVIDENCE-NO-SCRIPTS-RAN
  elif [ "$rc" = 0 ] && [ "$jail_blocked" = 0 ]; then
    verdict=OK
  elif [ "$jail_blocked" != 0 ]; then
    verdict=JAIL-INVOLVED
  elif [ "$net_err" != 0 ]; then
    verdict=NETWORK-ERROR-CHECK-EGRESS
  else
    verdict=FAILED-OTHER
  fi
  first_err=$(grep -viE '^\s*(WARN|warning|npm warn|gyp WARN)' "$log" 2>/dev/null \
    | grep -oiE 'nub build sandbox: blocked[^.]*|could not confine[^.]*|ENOTFOUND [a-z0-9.-]*|EACCES[^ ]*|node-pre-gyp ERR![^,]*|Cannot find module [^ ]*|fatal error: [^ ]*|× lifecycle script [a-z]* failed for [^ ]*' \
    | head -1)
  printf '%s\t%s\trc=%s\tscripts-ran=%s\tjail-lines=%s\tskipped-builds=%s\t%s\n' \
    "$name" "$verdict" "$rc" "$ran" "$jail_blocked" "$skipped" "${first_err:-—}" >> "$OUT"
  echo "  $name -> $verdict (rc=$rc, scripts-ran=$ran, jail-lines=$jail_blocked)"
  [ -n "$KEEP" ] && cp "$log" "$KEEP/$name.log" 2>/dev/null
  rm -rf "$home"
done

echo
echo "── ecosystem sweep, jail DEFAULT-ON, cold ──"
awk -F'\t' '{print $2}' "$OUT" | sort | uniq -c | sort -rn
echo "rows -> $OUT"
# The number the bar is about: projects where the JAIL was involved in a failure.
# Both buckets gate: a NETWORK-ERROR row may be a silent egress denial, and shipping default-on on the
# strength of a number that quietly excluded them would be the whole point missed.
bad=$(awk -F'\t' '$2=="JAIL-INVOLVED" || $2=="NETWORK-ERROR-CHECK-EGRESS"' "$OUT" | wc -l | tr -d ' ')
total=$(wc -l < "$OUT" | tr -d ' ')
confined=$(awk -F'\t' '{split($4,a,"="); if (a[2]+0 > 0) n++} END{print n+0}' "$OUT")
echo "projects where a lifecycle script actually ran (i.e. the jail was exercised): $confined / $total"
echo "jail-involved or possible-egress-denial failures: $bad / $total"
[ "$bad" = 0 ] || exit 1
