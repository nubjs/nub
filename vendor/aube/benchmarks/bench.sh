#!/usr/bin/env bash
set -euo pipefail

# Benchmark script comparing aube, pnpm, yarn (berry), npm, bun, deno,
# and vlt install performance.
#
# Prerequisites:
#   - aube built in release mode: cargo build --release
#   - benchmark dependencies from mise (use `mise run bench` or
#     `mise run bench:bump`; missing package managers are skipped with
#     a warning rather than failing the whole run)
#
# Usage:
#   mise run bench
#
# Environment variables:
#   WARMUP       — warmup runs before timing (default: 1)
#   RUNS         — timed runs per benchmark (default: 10). Applies to
#                  the fast tools (aube, bun, deno). Slower tools
#                  default to fewer runs so the matrix doesn't take
#                  forever: pnpm = vlt = ceil(RUNS/2),
#                  npm = yarn = ceil(RUNS/3).
#   RUNS_PNPM, RUNS_NPM, RUNS_YARN, RUNS_BUN, RUNS_AUBE, RUNS_DENO,
#   RUNS_VLT     — override the per-tool run count individually. Falls
#                  back to the defaults above when unset.
#   RESULTS_JSON — override the structured JSON output path
#   BENCH_TOOLS  — comma-separated tools to include
#                  (default: aube,bun,pnpm,npm,yarn,deno; vlt is
#                  temporarily disabled — its --frozen-lockfile still
#                  makes network requests, skewing results)
#   BENCH_SCENARIOS — comma-separated scenario keys to run
#                     (default: all)
#   BENCH_PHASES — set to 0 to skip aube phase timing samples
#
#   BENCH_HERMETIC=1 — route all registry traffic through a local
#                      Verdaccio instance pre-populated from npmjs. This
#                      is the default for mise tasks; leave it on so
#                      cold-cache numbers are not npmjs/CDN latency tests.
#                      First hermetic run warms the cache at
#                      ~/.cache/aube-bench/registry/; subsequent runs
#                      are fully offline. See benchmarks/hermetic.bash.
#   BENCH_BANDWIDTH  — optional throttle (e.g. `50mbit`, `6mbit`, bare
#                      integer bytes/s). Defaults to `500mbit` in mise
#                      tasks; routes traffic through a tiny token-bucket
#                      proxy in front of Verdaccio.
#   BENCH_LATENCY    — optional fixed response latency for the throttle
#                      proxy. Defaults to `50ms` in mise tasks.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
AUBE_BIN="$REPO_DIR/target/release/aube"
PNPM_BIN="$(command -v pnpm || true)"
YARN_BIN="$(command -v yarn || true)"
NPM_BIN="$(command -v npm || true)"
BUN_BIN="$(command -v bun || true)"
DENO_BIN="$(command -v deno || true)"
VLT_BIN="$(command -v vlt || true)"

BENCH_DIR="$(mktemp -d "${TMPDIR:-/tmp}/aube-bench.XXXXXX")"
WARMUP="${WARMUP:-1}"
RUNS="${RUNS:-10}"
# Slower tools take a real chunk of wall time per iteration; default
# pnpm to half the run count and npm/yarn to a third. Each is overridable.
RUNS_AUBE="${RUNS_AUBE:-$RUNS}"
RUNS_BUN="${RUNS_BUN:-$RUNS}"
RUNS_DENO="${RUNS_DENO:-$RUNS}"
RUNS_PNPM="${RUNS_PNPM:-$(((RUNS + 1) / 2))}"
RUNS_VLT="${RUNS_VLT:-$(((RUNS + 1) / 2))}"
RUNS_NPM="${RUNS_NPM:-$(((RUNS + 2) / 3))}"
RUNS_YARN="${RUNS_YARN:-$(((RUNS + 2) / 3))}"
BENCH_TOOLS="${BENCH_TOOLS:-aube,bun,pnpm,npm,yarn,deno}"
BENCH_SCENARIOS="${BENCH_SCENARIOS:-gvs-warm,gvs-cold,install-test}"
BENCH_PHASES="${BENCH_PHASES:-1}"

# ── Validation ──────────────────────────────────────────────────────────────

if ! command -v hyperfine &>/dev/null; then
	echo "error: hyperfine is required. Run via: mise run bench" >&2
	exit 1
fi

if [ ! -f "$AUBE_BIN" ]; then
	echo "error: aube release binary not found at $AUBE_BIN" >&2
	echo "Run: cargo build --release" >&2
	exit 1
fi

# ── Optional hermetic registry ─────────────────────────────────────────────
# BENCH_HERMETIC=1 routes all registry traffic through a local
# Verdaccio instance (populated from npmjs on first run, offline after).
# BENCH_BANDWIDTH=<rate> and BENCH_LATENCY=<delay> put a throttling
# proxy in front so cold-cache numbers reflect a simulated internet
# link rather than loopback disk speed. See benchmarks/hermetic.bash
# for the lifecycle details.

BENCH_REGISTRY_URL=""
if [ "${BENCH_HERMETIC:-0}" = "1" ]; then
	# shellcheck source=/dev/null
	source "$SCRIPT_DIR/hermetic.bash"
	hermetic_start
	trap 'hermetic_stop' EXIT
fi

# ── Per-tool configuration ─────────────────────────────────────────────────
# Build up the list of tools to include dynamically so the matrix
# gracefully skips any pm that isn't installed. Each tool gets its
# own project dir, HOME, store, and cache so the scenarios are
# hermetic per-tool.

TOOLS=()
TOOL_BINS=()
TOOL_PROJECTS=()
TOOL_HOMES=()
TOOL_STORES=()
TOOL_CACHES=()

register_tool() {
	local name=$1 bin=$2
	case ",$BENCH_TOOLS," in
	*,"$name",*) ;;
	*) return ;;
	esac
	if [ -z "$bin" ] || [ ! -x "$bin" ]; then
		echo "warning: $name not found on \$PATH — skipping" >&2
		return
	fi
	TOOLS+=("$name")
	TOOL_BINS+=("$bin")
	TOOL_PROJECTS+=("$BENCH_DIR/project-$name")
	TOOL_HOMES+=("$BENCH_DIR/home-$name")
	TOOL_STORES+=("$BENCH_DIR/store-$name")
	TOOL_CACHES+=("$BENCH_DIR/cache-$name")
}

run_scenario() {
	local name=$1
	case ",$BENCH_SCENARIOS," in
	*,"$name",*) ;;
	*) return ;;
	esac

	shift
	"$@"
}

# Order matters for the console output; keep aube first so the
# headline comparison is prominent and the rest follow alphabetically.
register_tool "aube" "$AUBE_BIN"
register_tool "bun" "$BUN_BIN"
register_tool "deno" "$DENO_BIN"
register_tool "pnpm" "$PNPM_BIN"
register_tool "npm" "$NPM_BIN"
register_tool "yarn" "$YARN_BIN"
register_tool "vlt" "$VLT_BIN"

echo "workdir: $BENCH_DIR"
# Capture each tool's reported --version string so generate-results.js
# can fold it into results.json. Some tools print extra text around
# the semver (e.g. `aube 1.0.0-beta.3 (...)`, `bun 1.3.12+...`); the
# sed pulls out the first token that looks like a semver so the JSON
# stays clean without the consumers having to re-parse it.
versions_file="$BENCH_DIR/versions.tsv"
: >"$versions_file"
for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	bin="${TOOL_BINS[$i]}"
	raw="$($bin --version 2>/dev/null || echo 'unknown')"
	version="$(printf '%s\n' "$raw" | head -n1 | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?' | head -n1)"
	[ -z "$version" ] && version="$raw"
	printf "%s\t%s\n" "$tool" "$version" >>"$versions_file"
	printf "%-5s %s  (%s)\n" "$tool:" "$bin" "$version"
done
node_version="$(node --version 2>/dev/null | sed 's/^v//')"
if [ -n "$node_version" ]; then
	printf "%s\t%s\n" "node" "$node_version" >>"$versions_file"
	printf "%-5s %s\n" "node:" "$node_version"
fi
export BENCH_VERSIONS_FILE="$versions_file"
echo ""

runs_for_tool() {
	case "$1" in
	aube) echo "$RUNS_AUBE" ;;
	bun) echo "$RUNS_BUN" ;;
	deno) echo "$RUNS_DENO" ;;
	pnpm) echo "$RUNS_PNPM" ;;
	npm) echo "$RUNS_NPM" ;;
	yarn) echo "$RUNS_YARN" ;;
	vlt) echo "$RUNS_VLT" ;;
	*) echo "$RUNS" ;;
	esac
}

# Per-tool lockfile filename (the name the pm writes into the project
# directory after `install`). Used to decide what to save after the
# populate step and where to copy it back for the "warm lockfile"
# scenarios.
lockfile_name_for() {
	case "$1" in
	aube) echo "aube-lock.yaml" ;;
	bun) echo "bun.lock" ;;
	deno) echo "deno.lock" ;;
	npm) echo "package-lock.json" ;;
	pnpm) echo "pnpm-lock.yaml" ;;
	yarn) echo "yarn.lock" ;;
	vlt) echo "vlt-lock.json" ;;
	*) echo "unknown" ;;
	esac
}

# ── Project setup ──────────────────────────────────────────────────────────

for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	dir="${TOOL_PROJECTS[$i]}"
	home="${TOOL_HOMES[$i]}"
	mkdir -p "$dir" "$home" "${TOOL_CACHES[$i]}"
	cp "$SCRIPT_DIR/fixture.package.json" "$dir/package.json"

	# pnpm reads storeDir / cacheDir from pnpm-workspace.yaml; the
	# other tools take them via CLI flags or env vars at command
	# time, so nothing to write on disk up front.
	if [ "$tool" = "pnpm" ]; then
		printf "storeDir: %s\ncacheDir: %s\n" "${TOOL_STORES[$i]}" "${TOOL_CACHES[$i]}" >"$dir/pnpm-workspace.yaml"
	fi

	# Yarn 4 ignores .npmrc for registry and only ships a PnP linker by
	# default. Drop a .yarnrc.yml that pins node-modules layout (so the
	# scenarios mirror what npm/pnpm/bun produce), routes the cache to
	# the isolated dir, disables telemetry, and turns off lifecycle
	# scripts so it matches the --ignore-scripts behavior we ask from
	# the other tools. The hermetic registry URL gets injected lower
	# down once we know BENCH_REGISTRY_URL.
	if [ "$tool" = "yarn" ]; then
		{
			printf "nodeLinker: node-modules\n"
			printf "cacheFolder: %s\n" "${TOOL_CACHES[$i]}"
			printf "enableGlobalCache: false\n"
			printf "enableTelemetry: false\n"
			printf "enableScripts: false\n"
			# Yarn 4 auto-enables immutable installs when it detects
			# `CI=true`, which makes the warm step refuse to create
			# the initial lockfile (YN0028) and crashes bench-refresh.
			# Pin the flag off so the warm install can populate the
			# lockfile + cache + store the same way it does locally.
			printf "enableImmutableInstalls: false\n"
		} >"$dir/.yarnrc.yml"
	fi

	# Hermetic mode: drop a .npmrc into both the project dir and the
	# isolated HOME so every PM resolves packages through the local
	# Verdaccio (or the throttle proxy in front of it) instead of
	# npmjs. Project-level .npmrc is honored by aube/pnpm/npm/bun/
	# deno/vlt and wins over HOME; HOME is a belt-and-suspenders
	# fallback for any command (like `aube add` after chdir) that
	# might look there first. Yarn 4 ignores .npmrc and reads the
	# registry from .yarnrc.yml instead, so we append it there.
	if [ -n "$BENCH_REGISTRY_URL" ]; then
		printf "registry=%s\n" "$BENCH_REGISTRY_URL" >"$dir/.npmrc"
		printf "registry=%s\n" "$BENCH_REGISTRY_URL" >"$home/.npmrc"
		if [ "$tool" = "yarn" ]; then
			printf "npmRegistryServer: \"%s\"\nunsafeHttpWhitelist:\n  - 127.0.0.1\n  - localhost\n" \
				"$BENCH_REGISTRY_URL" >>"$dir/.yarnrc.yml"
		fi
	fi
done

# Keep a pristine copy of package.json
cp "$SCRIPT_DIR/fixture.package.json" "$BENCH_DIR/original-package.json"

# ── Populate stores and caches ─────────────────────────────────────────────
# One warm install per tool so the lockfile + cache + store are all
# populated before the scenario matrix runs. Everything is hermetic
# (isolated HOME / cache / store), so this is safe to run in parallel
# in the future but we keep it serial for clear console output.
#
# Bracket the populate loop with uplink-enabled Verdaccio so any
# package the warm step missed (e.g. when a PM's resolution diverges
# between the warm fixture and the bench fixture) gets fetched from
# npmjs and cached locally. The cold config is restored afterwards so
# the timed scenarios remain hermetic.
if [ "${BENCH_HERMETIC:-0}" = "1" ]; then
	hermetic_use_warm_uplink
fi

for i in "${!TOOLS[@]}"; do
	tool="${TOOLS[$i]}"
	dir="${TOOL_PROJECTS[$i]}"
	bin="${TOOL_BINS[$i]}"
	home="${TOOL_HOMES[$i]}"
	store="${TOOL_STORES[$i]}"
	cache="${TOOL_CACHES[$i]}"
	lockfile_name=$(lockfile_name_for "$tool")
	echo "Populating store and cache for $tool..."
	# Wipe every known lockfile so an earlier failed run doesn't
	# leave a stale one behind that would fool the pm into a
	# different code path.
	rm -rf "$dir/node_modules" \
		"$dir/pnpm-lock.yaml" \
		"$dir/aube-lock.yaml" \
		"$dir/package-lock.json" \
		"$dir/yarn.lock" \
		"$dir/bun.lock" \
		"$dir/bun.lockb" \
		"$dir/deno.lock" \
		"$dir/vlt-lock.json"

	case "$tool" in
	aube)
		cd "$dir" && HOME="$home" XDG_CACHE_HOME="$cache" XDG_DATA_HOME="$home/.local/share" "$bin" install
		;;
	npm)
		# `--legacy-peer-deps` is the only way npm tolerates the
		# fixture's mixed peer-dep ranges (eslint 9 vs 8, etc.).
		# pnpm/aube handle this via `autoInstallPeers=true` by
		# default; using npm's strict mode here would just make
		# the populate step fail before we even reach the
		# scenarios. Yes, this is the classic "npm is stricter"
		# caveat you read in every benchmark footnote.
		cd "$dir" && HOME="$home" npm_config_cache="$cache" "$bin" install \
			--ignore-scripts --no-audit --no-fund --legacy-peer-deps
		;;
	pnpm)
		cd "$dir" && HOME="$home" "$bin" install --ignore-scripts --no-frozen-lockfile
		;;
	yarn)
		# Yarn 4 (berry). enableScripts/cacheFolder/nodeLinker are
		# already pinned in .yarnrc.yml, so we only need to ask for
		# a fresh install here.
		cd "$dir" && HOME="$home" "$bin" install
		;;
	bun)
		# Bun takes `--cache-dir` as a CLI flag and `BUN_INSTALL` as
		# the global install prefix. Point both at the hermetic temp
		# to keep it from touching `~/.bun`.
		cd "$dir" && HOME="$home" BUN_INSTALL="$home/.bun" "$bin" install \
			--cache-dir "$cache" --ignore-scripts --no-summary --force
		;;
	deno)
		# Deno 2 reads package.json and writes deno.lock + populates
		# node_modules. DENO_DIR is the per-tool cache and global
		# install location. Lifecycle scripts are skipped by default
		# (Deno requires explicit --allow-scripts to opt in).
		cd "$dir" && HOME="$home" DENO_DIR="$cache" "$bin" install --quiet
		;;
	vlt)
		# vlt respects npm_config_cache for its package cache and
		# reads .npmrc for the registry. Skips lifecycle scripts by
		# default unless an allowlist is configured.
		cd "$dir" && HOME="$home" npm_config_cache="$cache" "$bin" install
		;;
	esac

	if [ ! -f "$dir/$lockfile_name" ]; then
		echo "error: $lockfile_name was not created for $tool in $dir" >&2
		exit 1
	fi
	cp "$dir/$lockfile_name" "$BENCH_DIR/saved-lockfile-$tool"
done

if [ "${BENCH_HERMETIC:-0}" = "1" ]; then
	hermetic_use_no_uplink
fi

# ── Helper ─────────────────────────────────────────────────────────────────
#
# Each bench scenario is driven by:
#
#   - one shared `prepare_tpl` that sets the on-disk state for the
#     tool's project dir (wiping `node_modules`, dropping back the
#     saved lockfile, etc.)
#   - a per-tool command template looked up by `cmd_template`
#
# Template placeholders:
#   {project}       — project directory
#   {bin}           — tool binary
#   {home}          — isolated HOME directory
#   {store}         — store directory (pnpm/aube)
#   {cache}         — cache directory
#   {lockfile}      — saved lockfile path (source of the copy)
#   {lockfile_dest} — per-tool lockfile destination in the project
#                     directory (matches the pm's native filename)

expand_template() {
	local tpl=$1 project=$2 bin=$3 home=$4 store=$5 cache=$6 lockfile=$7 lockfile_dest=$8
	tpl="${tpl//\{project\}/$project}"
	tpl="${tpl//\{bin\}/$bin}"
	tpl="${tpl//\{home\}/$home}"
	tpl="${tpl//\{store\}/$store}"
	tpl="${tpl//\{cache\}/$cache}"
	tpl="${tpl//\{lockfile\}/$lockfile}"
	tpl="${tpl//\{lockfile_dest\}/$lockfile_dest}"
	echo "$tpl"
}

# Minimum publish-age gate, in minutes. aube defaults to 1440 (24h)
# as a supply-chain mitigation — the resolver skips versions newer
# than this window. The default forces aube to fetch the full
# (non-corgi) packument format so it can read the per-version `time`
# map; corgi omits `time` on npmjs.org. Most modern PMs support an
# equivalent flag, each with their own unit:
#
#   aube  minimumReleaseAge          (minutes; default 1440)
#   npm   --min-release-age          (days)
#   pnpm  --config.minimum-release-age (minutes)
#   bun   --minimum-release-age      (seconds)
#   deno  --minimum-dependency-age   (minutes, marked Unstable)
#   yarn  not supported
#   vlt   not investigated (currently disabled in BENCH_TOOLS anyway)
#
# Pinning all supported PMs to the same value makes the bench an
# apples-to-apples comparison — otherwise aube alone pays the
# full-packument cost (5x larger response on @types/node and similar
# heavily-versioned packuments) while bun/pnpm cruise on corgi.
#
# Override via `BENCH_MIN_RELEASE_AGE_MINUTES=0` to disable the gate
# across all PMs (useful for measuring raw resolver speed without
# the security-feature axis).
MIN_RELEASE_AGE_MINUTES="${BENCH_MIN_RELEASE_AGE_MINUTES:-1440}"
MIN_RELEASE_AGE_SECONDS=$((MIN_RELEASE_AGE_MINUTES * 60))
# npm uses days as the unit. Round up so the gate is at least as
# strict as aube's, never weaker. (60*24 = 1440 → 1 day exactly.)
MIN_RELEASE_AGE_DAYS=$(((MIN_RELEASE_AGE_MINUTES + 60 * 24 - 1) / (60 * 24)))

# Per-tool boilerplate factored out of the `CMDS` declarations below.
# Every bun invocation threads the same hermetic environment
# (isolated `HOME`, `BUN_INSTALL`, `--cache-dir`, `--ignore-scripts`,
# `--no-summary`) so the scenarios only have to spell out the
# install-mode flags that actually vary per scenario.
#
# `--minimum-release-age` is bun's name for the same supply-chain
# gate aube defaults on; matching the value here keeps the bench
# from advantaging bun by silently skipping work aube does.
BUN_BASE="HOME={home} BUN_INSTALL={home}/.bun {bin} install --cache-dir {cache} --ignore-scripts --no-summary --minimum-release-age=${MIN_RELEASE_AGE_SECONDS}"

# aube reads the global store root from `$XDG_DATA_HOME/aube/store`
# (falling back to `$HOME/.local/share/aube/store`). We must pin
# `XDG_DATA_HOME` alongside `HOME` and `XDG_CACHE_HOME` — otherwise
# a host that already has `XDG_DATA_HOME` set in its environment
# would leak the benchmark's store out of the isolated `{home}`,
# and `COLD_WIPE` wouldn't find it to clean up between iterations.
# `npm_config_minimum_release_age` propagates the bench's
# minimum-release-age value into aube (aube reads this env var via
# its npm-compatible settings layer). Without it, aube uses its
# compiled-in default (1440) regardless of
# `BENCH_MIN_RELEASE_AGE_MINUTES`, silently breaking the
# apples-to-apples guarantee for any non-default override.
AUBE_ENV="HOME={home} XDG_CACHE_HOME={cache} XDG_DATA_HOME={home}/.local/share npm_config_minimum_release_age=${MIN_RELEASE_AGE_MINUTES}"

# Per-scenario AUBE_ENV variant that pins aube's global virtual store on
# via the `enableGlobalVirtualStore` setting's auto-synthesized env-var
# alias (`npm_config_<snake_case>` — see `aube-settings/build.rs`).
# Using an env var rather than `--enable-gvs` means scenarios that go
# through `aube test` (which triggers auto-install internally) get the
# same forcing as direct `aube install` calls. The setting wins over
# `Linker::new`'s `CI` heuristic, so GitHub Actions' inherited `CI=true`
# cannot silently flip the mode.
AUBE_ENV_GVS_ON="$AUBE_ENV npm_config_enable_global_virtual_store=true"

# Scenario keys describe what's on disk before the run. Every install
# scenario assumes a committed lockfile is present; the axes are
# cache/store warmth. The "install-test" scenario measures install +
# script dispatch end-to-end.

cmd_template() {
	case "$1:$2" in
	gvs-warm:aube | gvs-cold:aube)
		echo "cd {project} && $AUBE_ENV_GVS_ON {bin} install --frozen-lockfile >/dev/null 2>&1"
		;;
	gvs-warm:bun | gvs-cold:bun)
		echo "cd {project} && $BUN_BASE --frozen-lockfile >/dev/null 2>&1"
		;;
	gvs-warm:npm)
		echo "cd {project} && HOME={home} npm_config_cache={cache} {bin} ci --ignore-scripts --no-audit --no-fund --legacy-peer-deps --prefer-offline --min-release-age=${MIN_RELEASE_AGE_DAYS} >/dev/null 2>&1"
		;;
	gvs-warm:pnpm | gvs-cold:pnpm)
		echo "cd {project} && HOME={home} {bin} install --frozen-lockfile --ignore-scripts --config.minimum-release-age=${MIN_RELEASE_AGE_MINUTES} >/dev/null 2>&1"
		;;
	gvs-warm:yarn | gvs-cold:yarn)
		# Yarn 4: --immutable replaces --frozen-lockfile and aborts
		# if the lockfile or cache would change. Scripts/cache/linker
		# settings are already pinned in .yarnrc.yml.
		echo "cd {project} && HOME={home} {bin} install --immutable >/dev/null 2>&1"
		;;
	gvs-warm:deno | gvs-cold:deno)
		# Deno 2: --frozen errors out if the lockfile would change,
		# the equivalent of --frozen-lockfile elsewhere. Lifecycle
		# scripts are off unless --allow-scripts is passed.
		# `--minimum-dependency-age` is flagged "Unstable" in deno's
		# help but the flag itself parses fine; takes minutes.
		echo "cd {project} && HOME={home} DENO_DIR={cache} {bin} install --frozen --quiet --minimum-dependency-age=${MIN_RELEASE_AGE_MINUTES} >/dev/null 2>&1"
		;;
	gvs-warm:vlt | gvs-cold:vlt)
		# vlt's --frozen-lockfile mirrors pnpm/npm/aube semantics: refuse
		# to re-resolve and error out if vlt-lock.json would change.
		# Without it vlt would silently treat the install as a fresh
		# resolve, which is not what the other tools measure here.
		echo "cd {project} && HOME={home} npm_config_cache={cache} {bin} install --frozen-lockfile >/dev/null 2>&1"
		;;
	gvs-cold:npm)
		echo "cd {project} && HOME={home} npm_config_cache={cache} {bin} ci --ignore-scripts --no-audit --no-fund --legacy-peer-deps --min-release-age=${MIN_RELEASE_AGE_DAYS} >/dev/null 2>&1"
		;;
	install-test:aube)
		echo "cd {project} && $AUBE_ENV_GVS_ON {bin} test >/dev/null 2>&1"
		;;
	install-test:bun)
		echo "cd {project} && $BUN_BASE --frozen-lockfile >/dev/null 2>&1 && HOME={home} BUN_INSTALL={home}/.bun {bin} run test >/dev/null 2>&1"
		;;
	install-test:npm)
		echo "cd {project} && HOME={home} npm_config_cache={cache} {bin} install-test --ignore-scripts --no-audit --no-fund --legacy-peer-deps --prefer-offline --min-release-age=${MIN_RELEASE_AGE_DAYS} >/dev/null 2>&1"
		;;
	install-test:pnpm)
		echo "cd {project} && HOME={home} {bin} install-test --frozen-lockfile --ignore-scripts --config.minimum-release-age=${MIN_RELEASE_AGE_MINUTES} >/dev/null 2>&1"
		;;
	install-test:yarn)
		echo "cd {project} && HOME={home} {bin} install --immutable >/dev/null 2>&1 && HOME={home} {bin} test >/dev/null 2>&1"
		;;
	install-test:deno)
		echo "cd {project} && HOME={home} DENO_DIR={cache} {bin} install --frozen --quiet --minimum-dependency-age=${MIN_RELEASE_AGE_MINUTES} >/dev/null 2>&1 && HOME={home} DENO_DIR={cache} {bin} task --quiet test >/dev/null 2>&1"
		;;
	install-test:vlt)
		echo "cd {project} && HOME={home} npm_config_cache={cache} {bin} install --frozen-lockfile >/dev/null 2>&1 && HOME={home} npm_config_cache={cache} {bin} run test >/dev/null 2>&1"
		;;
	esac
}

run_bench() {
	local bench_name=$1
	local prepare_tpl=$2

	for i in "${!TOOLS[@]}"; do
		local tool="${TOOLS[$i]}"
		local project="${TOOL_PROJECTS[$i]}"
		local bin="${TOOL_BINS[$i]}"
		local home="${TOOL_HOMES[$i]}"
		local store="${TOOL_STORES[$i]}"
		local cache="${TOOL_CACHES[$i]}"
		local lockfile="$BENCH_DIR/saved-lockfile-$tool"
		local lockfile_dest
		lockfile_dest="$project/$(lockfile_name_for "$tool")"

		local cmd_tpl
		cmd_tpl=$(cmd_template "$bench_name" "$tool")
		if [ -z "$cmd_tpl" ]; then
			echo "warning: no $bench_name command for $tool — skipping" >&2
			continue
		fi

		local prepare
		prepare=$(expand_template "$prepare_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		local cmd
		cmd=$(expand_template "$cmd_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		local tool_runs
		tool_runs=$(runs_for_tool "$tool")
		echo ""
		echo "  $tool:"
		hyperfine \
			--warmup "$WARMUP" \
			--runs "$tool_runs" \
			--ignore-failure \
			--prepare "$prepare" \
			--command-name "$tool" \
			"$cmd" \
			--export-json "$BENCH_DIR/${bench_name}-${tool}.json" ||
			true
	done
}

# Like `run_bench`, but times the *second* invocation of the tool's
# command — the prepare step wipes node_modules, restores the saved
# lockfile, and runs the same command once so the timed iteration
# starts from a "node_modules is already valid" state.
#
# Used by the install-test scenario to measure the "I've installed,
# now I just want to re-run my tests" developer loop rather than the
# "fresh checkout + install" loop (which `gvs-warm` already covers).
run_bench_preinstall() {
	local bench_name=$1

	for i in "${!TOOLS[@]}"; do
		local tool="${TOOLS[$i]}"
		local project="${TOOL_PROJECTS[$i]}"
		local bin="${TOOL_BINS[$i]}"
		local home="${TOOL_HOMES[$i]}"
		local store="${TOOL_STORES[$i]}"
		local cache="${TOOL_CACHES[$i]}"
		local lockfile="$BENCH_DIR/saved-lockfile-$tool"
		local lockfile_dest
		lockfile_dest="$project/$(lockfile_name_for "$tool")"

		local cmd_tpl
		cmd_tpl=$(cmd_template "$bench_name" "$tool")
		if [ -z "$cmd_tpl" ]; then
			echo "warning: no $bench_name command for $tool — skipping" >&2
			continue
		fi

		local cmd
		cmd=$(expand_template "$cmd_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		local warm_prep
		warm_prep=$(expand_template "$WARM_PREP" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		# Prepare: wipe + restore lockfile, then run the same command
		# once untimed so the tool's install phase populates
		# node_modules (and `.aube-state` for aube). The timed
		# iteration then re-runs the command against the settled
		# state — the developer-loop "run my tests again" case.
		local prepare="$warm_prep && $cmd"

		local tool_runs
		tool_runs=$(runs_for_tool "$tool")
		echo ""
		echo "  $tool:"
		hyperfine \
			--warmup "$WARMUP" \
			--runs "$tool_runs" \
			--ignore-failure \
			--prepare "$prepare" \
			--command-name "$tool" \
			"$cmd" \
			--export-json "$BENCH_DIR/${bench_name}-${tool}.json" ||
			true
	done
}

PHASES_FILE="$BENCH_DIR/aube-install-phases.jsonl"
: >"$PHASES_FILE"

run_aube_phase_bench() {
	local bench_name=$1
	local prepare_tpl=$2

	for i in "${!TOOLS[@]}"; do
		local tool="${TOOLS[$i]}"
		[ "$tool" = "aube" ] || continue

		local project="${TOOL_PROJECTS[$i]}"
		local bin="${TOOL_BINS[$i]}"
		local home="${TOOL_HOMES[$i]}"
		local store="${TOOL_STORES[$i]}"
		local cache="${TOOL_CACHES[$i]}"
		local lockfile="$BENCH_DIR/saved-lockfile-$tool"
		local lockfile_dest
		lockfile_dest="$project/$(lockfile_name_for "$tool")"

		local prepare
		prepare=$(expand_template "$prepare_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")

		local cmd_tpl
		cmd_tpl=$(cmd_template "$bench_name" "$tool")
		local cmd
		cmd=$(expand_template "$cmd_tpl" "$project" "$bin" "$home" "$store" "$cache" "$lockfile" "$lockfile_dest")
		# Inject phase-timing env vars after the `cd {project} && `
		# prefix. We can't use `${cmd/&& /&& ...}` here: bash 5.2+
		# treats `&` in the replacement of `${var/pat/repl}` as a
		# backreference to the matched pattern (sed-like), so each
		# unescaped `&` expands to the matched `&& ` and the result
		# becomes `cd <p> && && ...` which fails at eval time.
		# Escaping with `\&` works on 5.2+ but emits literal backslashes
		# on bash 3.2 (macOS dev shell). Splitting around `&& ` avoids
		# both traps and works the same on every bash we care about.
		cmd="${cmd%%&& *}&& AUBE_BENCH_PHASES_FILE=$PHASES_FILE AUBE_BENCH_SCENARIO=$bench_name ${cmd#*&& }"

		echo "  $bench_name"
		if ! eval "$prepare"; then
			echo "warning: phase timing prepare failed for $bench_name - skipping sample" >&2
			continue
		fi
		if ! eval "$cmd"; then
			echo "warning: phase timing run failed for $bench_name - skipping sample" >&2
			continue
		fi
	done
}

# Directories to wipe in cold scenarios. Each pm has its own cache /
# store layout, so we reset everything we know about to guarantee
# a fresh download on every iteration.
COLD_WIPE='{store} {cache} {home}/.pnpm-store {home}/.local/share/aube {home}/.npm {home}/.yarn {home}/.bun {home}/.cache/aube {home}/.cache/yarn {home}/.cache/bun {home}/.cache/deno {home}/.cache/vlt {home}/.config/vlt {home}/Library/Caches/deno'

# Warm-cache lockfile restore: wipe the project-local state (lockfile
# + node_modules) and drop the saved lockfile back. Uses the per-tool
# `lockfile_dest` placeholder so each pm gets its native filename.
WARM_PREP="rm -rf {project}/node_modules {project}/pnpm-lock.yaml {project}/aube-lock.yaml {project}/package-lock.json {project}/yarn.lock {project}/bun.lock {project}/bun.lockb {project}/deno.lock {project}/vlt-lock.json && cp {lockfile} {lockfile_dest}"
COLD_PREP="rm -rf {project}/node_modules {project}/pnpm-lock.yaml {project}/aube-lock.yaml {project}/package-lock.json {project}/yarn.lock {project}/bun.lock {project}/bun.lockb {project}/deno.lock {project}/vlt-lock.json $COLD_WIPE && mkdir -p {home} && cp {lockfile} {lockfile_dest}"

# ── Benchmark 1: Fresh install, warm cache ─────────────────────────────────
# Lockfile present, node_modules deleted, store and cache warm.
# Pins aube's default local global virtual store behavior so GitHub
# Actions' inherited CI=true environment cannot silently turn this into
# per-project mode.

echo ""
echo "━━━ Benchmark 1: Fresh install (warm cache) ━━━"
run_scenario "gvs-warm" run_bench "gvs-warm" "$WARM_PREP"

# ── Benchmark 2: Fresh install, cold cache ─────────────────────────────────
# Lockfile present, but store and cache are empty.
# Measures fetch-from-registry + import + link/materialization work.

echo ""
echo "━━━ Benchmark 2: Fresh install (cold cache) ━━━"
run_scenario "gvs-cold" run_bench "gvs-cold" "$COLD_PREP"

# ── Aube phase timing sample ───────────────────────────────────────────────
# Hyperfine owns stdout/stderr and times whole commands. For attribution,
# run aube once per install-shaped scenario with AUBE_BENCH_PHASES_FILE
# enabled so the binary writes structured resolve/fetch/link/script/state
# timings to JSONL, then summarize it at the end.

echo ""
echo "━━━ Aube install phase timings ━━━"
if [ "$BENCH_PHASES" != "0" ]; then
	run_scenario "gvs-warm" run_aube_phase_bench "gvs-warm" "$WARM_PREP"
	run_scenario "gvs-cold" run_aube_phase_bench "gvs-cold" "$COLD_PREP"
fi

# ── Benchmark 3: install + run test (developer loop) ───────────────────────
# Warm store+cache, lockfile present, node_modules *already* populated.
# Models the developer-loop case: "I've installed, now I keep re-running
# my tests." Each iteration's prepare runs the full install-test command
# once (untimed) so node_modules and any tool-specific state files are
# valid, then the timed iteration re-runs the same command. Tools with a
# state-based short-circuit (aube's .aube-state) skip install entirely
# on the timed run; tools without one still pay for lockfile revalidation.

echo ""
echo "━━━ Benchmark 3: install + run test (already installed) ━━━"
run_scenario "install-test" run_bench_preinstall "install-test"

# ── Summary ────────────────────────────────────────────────────────────────

RESULTS_MD="$BENCH_DIR/results.md"

echo ""
echo "━━━ Results ━━━"
TOOLS_CSV=$(
	IFS=,
	echo "${TOOLS[*]}"
)
BENCH_TOOLS="$TOOLS_CSV" BENCH_SCENARIOS="$BENCH_SCENARIOS" node "$SCRIPT_DIR/generate-results.js" "$BENCH_DIR" "$RESULTS_MD"
if [ -s "$PHASES_FILE" ]; then
	echo ""
	node "$SCRIPT_DIR/generate-phase-results.mjs" "$PHASES_FILE" "$BENCH_DIR/aube-install-phases.md"
fi
echo ""
echo "Results saved to: $RESULTS_MD"
echo ""
echo "Temp directory kept at: $BENCH_DIR"
echo "Remove with: rm -rf $BENCH_DIR"
