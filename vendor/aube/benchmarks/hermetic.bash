#!/usr/bin/env bash
# Hermetic benchmark registry lifecycle.
#
# Sourced by benchmarks/bench.sh when BENCH_HERMETIC=1. Exposes:
#
#   hermetic_start    — ensures the registry cache is warm, starts
#                       Verdaccio (no uplink), optionally starts a
#                       throttling proxy in front, and exports
#                       BENCH_REGISTRY_URL.
#   hermetic_stop     — tears both processes down. Idempotent; safe to
#                       call from an EXIT trap.
#
# Configuration (all env-driven):
#
#   BENCH_HERMETIC_CACHE  — persistent cache dir (default:
#                           ~/.cache/aube-bench/registry). Holds the
#                           Verdaccio storage and a `.warmed` sentinel.
#                           Wipe it to force a re-warm from npmjs.
#   BENCH_VERDACCIO_PORT  — localhost port for Verdaccio
#                           (default: 4874; distinct from test/registry's
#                           4873 so the two can coexist).
#   BENCH_BANDWIDTH       — optional throttle, e.g. `50mbit`, `6mbit`,
#                           `6250000` (bytes/s as a bare integer).
#                           When set, traffic goes through
#                           throttle-proxy.mjs instead of direct.
#   BENCH_LATENCY         — optional fixed response latency for the
#                           throttle proxy, e.g. `50ms`.
#   BENCH_PROXY_PORT      — localhost port for the throttle proxy
#                           (default: 4875).
#
# Shellcheck disables are scoped tight — this file is sourced by
# bench.sh, so top-level vars are intentionally not local.

# Resolve this file's directory. Prefer bench.sh's $SCRIPT_DIR when
# we're being sourced from it, and fall back to $BASH_SOURCE otherwise
# (manual `source benchmarks/hermetic.bash` from the repo root, CI ad
# hoc checks, etc.). $BASH_SOURCE is array-indexed in bash but some
# harnesses deliver it as a plain string, so we defensively coalesce.
if [ -n "${SCRIPT_DIR:-}" ] && [ -f "$SCRIPT_DIR/hermetic.bash" ]; then
	HERMETIC_DIR="$SCRIPT_DIR"
else
	_hermetic_src="${BASH_SOURCE[0]:-${BASH_SOURCE:-}}"
	if [ -n "$_hermetic_src" ] && [ -f "$_hermetic_src" ]; then
		# shellcheck disable=SC2155
		HERMETIC_DIR="$(cd "$(dirname "$_hermetic_src")" && pwd)"
	else
		echo "ERROR: hermetic.bash could not resolve its own directory." >&2
		echo "  Set SCRIPT_DIR to the benchmarks/ dir before sourcing." >&2
		# shellcheck disable=SC2317  # reachable via source-from-stdin or unusual harness invocations
		return 1 2>/dev/null || exit 1
	fi
	unset _hermetic_src
fi

BENCH_HERMETIC_CACHE="${BENCH_HERMETIC_CACHE:-$HOME/.cache/aube-bench/registry}"
BENCH_VERDACCIO_PORT="${BENCH_VERDACCIO_PORT:-4874}"
BENCH_PROXY_PORT="${BENCH_PROXY_PORT:-4875}"

HERMETIC_STORAGE="$BENCH_HERMETIC_CACHE/storage"
# Sentinel name carries a generation tag so adding/removing PMs in the
# default warm set automatically invalidates an existing warmed cache.
# Bump when the default BENCH_TOOLS set changes.
HERMETIC_WARMED_SENTINEL="$BENCH_HERMETIC_CACHE/.warmed.v3"
HERMETIC_LOG="$BENCH_HERMETIC_CACHE/verdaccio.log"
HERMETIC_CONFIG_WARM="$HERMETIC_DIR/registry/config.warm.yaml"
HERMETIC_CONFIG_COLD="$HERMETIC_DIR/registry/config.yaml"

# Install verdaccio globally if it isn't on PATH. Pinned to v6 to match
# test/registry/start.bash — both are meant to track the same upstream.
_hermetic_ensure_verdaccio() {
	if command -v verdaccio >/dev/null 2>&1; then
		return 0
	fi
	echo "Installing verdaccio..." >&2
	npm install --global verdaccio@6 2>&1 | tail -1 >&2
}

# Wait for Verdaccio to answer HTTP on $port. Returns 1 after ~30s.
_hermetic_wait_ready() {
	local port=$1
	local retries=60
	while ! curl -s "http://127.0.0.1:${port}/" >/dev/null 2>&1; do
		retries=$((retries - 1))
		if [ "$retries" -le 0 ]; then
			echo "ERROR: Verdaccio failed to start on port $port" >&2
			return 1
		fi
		sleep 0.5
	done
}

# Start Verdaccio with the given config file on BENCH_VERDACCIO_PORT.
# Writes PID into $HERMETIC_VERDACCIO_PID (exported so hermetic_stop
# can see it from an EXIT trap).
_hermetic_start_verdaccio() {
	local config=$1
	mkdir -p "$HERMETIC_STORAGE"

	# Work inside the cache dir so Verdaccio's `storage: ./storage`
	# resolves to $HERMETIC_STORAGE. We can't point at the in-repo
	# config directly because Verdaccio resolves `storage:` relative
	# to the config file, so we copy both configs next to the storage
	# dir at startup.
	cp "$config" "$BENCH_HERMETIC_CACHE/config.yaml"

	verdaccio \
		--config "$BENCH_HERMETIC_CACHE/config.yaml" \
		--listen "127.0.0.1:$BENCH_VERDACCIO_PORT" \
		>"$HERMETIC_LOG" 2>&1 &
	HERMETIC_VERDACCIO_PID=$!
	export HERMETIC_VERDACCIO_PID

	if ! _hermetic_wait_ready "$BENCH_VERDACCIO_PORT"; then
		echo "ERROR: Verdaccio log ($HERMETIC_LOG):" >&2
		tail -40 "$HERMETIC_LOG" >&2 || true
		kill "$HERMETIC_VERDACCIO_PID" 2>/dev/null || true
		return 1
	fi
}

_hermetic_stop_verdaccio() {
	if [ -n "${HERMETIC_VERDACCIO_PID:-}" ]; then
		kill "$HERMETIC_VERDACCIO_PID" 2>/dev/null || true
		wait "$HERMETIC_VERDACCIO_PID" 2>/dev/null || true
		unset HERMETIC_VERDACCIO_PID
	fi
}

# Populate the Verdaccio storage from npmjs on first use. Idempotent
# via the `.warmed` sentinel. Running the warm step requires network;
# subsequent benchmark runs are fully offline.
#
# Warming runs one install per PM (aube, bun, pnpm, npm, yarn, deno)
# so the cache is the *union* of every tool's resolution set —
# each resolver picks its preferred versions independently (e.g. aube
# may pick `get-intrinsic@1.3.1` which drags in `async-function` /
# `async-generator-function` while pnpm picks an earlier version
# without those deps). A single-PM warm leaves 404-holes the no-uplink
# bench then falls into. Individual tool failures during warm are
# logged and tolerated; the later per-tool populate in bench.sh is
# authoritative about what each tool actually needs.
#
# aube is skipped when `$AUBE_BIN` isn't built yet, so warming is still
# bootstrap-safe for CI flows that warm before compiling aube.
_hermetic_warm() {
	# Non-default BENCH_TOOLS gets its own sentinel; the default
	# sentinel cannot cover a tool set that includes anything outside
	# the default warm pass (e.g. re-enabling vlt), so don't fall back
	# to it.
	local warm_sentinel="$HERMETIC_WARMED_SENTINEL"
	if [ "${BENCH_TOOLS:-aube,bun,pnpm,npm,yarn,deno}" != "aube,bun,pnpm,npm,yarn,deno" ]; then
		warm_sentinel="$HERMETIC_STORAGE/.warmed.${BENCH_TOOLS//[^A-Za-z0-9_.-]/_}"
	fi

	if [ -f "$warm_sentinel" ]; then
		return 0
	fi

	echo "Warming hermetic registry cache at $HERMETIC_STORAGE ..." >&2
	echo "  (one-time network fetch; subsequent runs are offline)" >&2

	_hermetic_ensure_verdaccio
	if ! _hermetic_start_verdaccio "$HERMETIC_CONFIG_WARM"; then
		return 1
	fi

	local warm_root
	warm_root=$(mktemp -d "${TMPDIR:-/tmp}/aube-bench-warm.XXXXXX")
	cp "$HERMETIC_DIR/fixture.package.json" "$warm_root/base-package.json"

	local reg="http://127.0.0.1:$BENCH_VERDACCIO_PORT"
	_warm_one() {
		local pm=$1 bin=$2
		shift 2
		case ",${BENCH_TOOLS:-}," in
		,, | *,"$pm",*) ;;
		*) return 0 ;;
		esac
		if [ -z "$bin" ] || { [ ! -x "$bin" ] && ! command -v "$bin" >/dev/null 2>&1; }; then
			echo "  skip $pm (not available)" >&2
			return 0
		fi
		echo "  warming with $pm ..." >&2
		local pm_dir="$warm_root/$pm"
		mkdir -p "$pm_dir/home"
		cp "$warm_root/base-package.json" "$pm_dir/package.json"
		# Pin registry via both `.npmrc` (project + home) and
		# `npm_config_registry` — aube reads `.npmrc` and does not
		# honor the env var, while npm honors either. Yarn 4 ignores
		# `.npmrc` entirely and reads the registry from
		# `.yarnrc.yml`, so the yarn warm step drops one alongside.
		# Without the `.npmrc` files aube's warm install silently hits
		# npmjs directly and leaves holes in the Verdaccio cache.
		printf 'registry=%s\n' "$reg" >"$pm_dir/.npmrc"
		printf 'registry=%s\n' "$reg" >"$pm_dir/home/.npmrc"
		if [ "$pm" = "yarn" ]; then
			{
				printf 'nodeLinker: node-modules\n'
				printf 'enableTelemetry: false\n'
				printf 'enableScripts: false\n'
				printf 'enableGlobalCache: false\n'
				printf 'cacheFolder: "%s/yarn-cache"\n' "$pm_dir/home"
				printf 'npmRegistryServer: "%s"\n' "$reg"
				printf 'unsafeHttpWhitelist:\n  - 127.0.0.1\n  - localhost\n'
				# Yarn 4 auto-enables immutable installs when `CI=true`,
				# refusing to create the initial `yarn.lock` and failing
				# the warm step with YN0028. Without this the warm cache
				# misses transitive tarballs (e.g. node-gyp), and later
				# bench steps that ask for them 404 against Verdaccio.
				# Sibling fix for the bench.sh-side yarnrc in PR #594.
				printf 'enableImmutableInstalls: false\n'
			} >"$pm_dir/.yarnrc.yml"
		fi
		if ! (cd "$pm_dir" && HOME="$pm_dir/home" \
			npm_config_registry="$reg" \
			npm_config_cache="$pm_dir/home/.npm-cache" \
			DENO_DIR="$pm_dir/home/.deno" \
			BUN_INSTALL="$pm_dir/home/.bun" \
			"$@" >"$pm_dir/warm.log" 2>&1); then
			echo "  WARN: warm with $pm failed (continuing — see $pm_dir/warm.log)" >&2
			return 0
		fi
	}

	# Per-tool install invocations mirror bench.sh's populate step so
	# the warmed cache contains exactly the tarballs each tool will
	# later ask for.
	_warm_one aube "${AUBE_BIN:-}" "${AUBE_BIN:-aube}" install --ignore-scripts
	_warm_one bun "$(command -v bun || echo)" bun install --ignore-scripts --no-summary
	_warm_one pnpm "$(command -v pnpm || echo)" pnpm install --ignore-scripts --no-frozen-lockfile
	_warm_one npm "$(command -v npm || echo)" npm install --ignore-scripts --no-audit --no-fund --legacy-peer-deps
	_warm_one yarn "$(command -v yarn || echo)" yarn install
	_warm_one deno "$(command -v deno || echo)" deno install --quiet
	_warm_one vlt "$(command -v vlt || echo)" vlt install

	unset -f _warm_one
	rm -rf "$warm_root"
	_hermetic_stop_verdaccio
	: >"$warm_sentinel"
	echo "Hermetic registry cache warmed." >&2
}

# Optional throttling proxy lifecycle. The proxy is a ~100-line Node
# script with zero deps (benchmarks/throttle-proxy.mjs). We invoke it
# with the upstream URL + rate; it prints "ready" to stdout once
# listening so we know when to return.
_hermetic_start_proxy() {
	local rate=$1
	local upstream="http://127.0.0.1:$BENCH_VERDACCIO_PORT"

	node "$HERMETIC_DIR/throttle-proxy.mjs" \
		--port "$BENCH_PROXY_PORT" \
		--upstream "$upstream" \
		--rate "$rate" \
		--latency "${BENCH_LATENCY:-0}" \
		>"$BENCH_HERMETIC_CACHE/proxy.log" 2>&1 &
	HERMETIC_PROXY_PID=$!
	export HERMETIC_PROXY_PID

	# Wait for the proxy to come up (it proxies to Verdaccio, which is
	# already ready, so this is usually instantaneous).
	local retries=40
	while ! curl -s "http://127.0.0.1:$BENCH_PROXY_PORT/" >/dev/null 2>&1; do
		retries=$((retries - 1))
		if [ "$retries" -le 0 ]; then
			echo "ERROR: throttle proxy failed to start on port $BENCH_PROXY_PORT" >&2
			tail -40 "$BENCH_HERMETIC_CACHE/proxy.log" >&2 || true
			kill "$HERMETIC_PROXY_PID" 2>/dev/null || true
			return 1
		fi
		sleep 0.25
	done
}

_hermetic_stop_proxy() {
	if [ -n "${HERMETIC_PROXY_PID:-}" ]; then
		kill "$HERMETIC_PROXY_PID" 2>/dev/null || true
		wait "$HERMETIC_PROXY_PID" 2>/dev/null || true
		unset HERMETIC_PROXY_PID
	fi
}

hermetic_start() {
	mkdir -p "$BENCH_HERMETIC_CACHE"
	_hermetic_ensure_verdaccio
	if ! _hermetic_warm; then
		return 1
	fi

	if ! _hermetic_start_verdaccio "$HERMETIC_CONFIG_COLD"; then
		return 1
	fi

	if [ -n "${BENCH_BANDWIDTH:-}" ]; then
		if ! _hermetic_start_proxy "$BENCH_BANDWIDTH"; then
			_hermetic_stop_verdaccio
			return 1
		fi
		export BENCH_REGISTRY_URL="http://127.0.0.1:$BENCH_PROXY_PORT"
		echo "Hermetic registry: $BENCH_REGISTRY_URL (throttled to $BENCH_BANDWIDTH, latency ${BENCH_LATENCY:-0}, upstream :$BENCH_VERDACCIO_PORT)" >&2
	else
		export BENCH_REGISTRY_URL="http://127.0.0.1:$BENCH_VERDACCIO_PORT"
		echo "Hermetic registry: $BENCH_REGISTRY_URL (unthrottled)" >&2
	fi
}

# Stop proxy before Verdaccio — the proxy holds keep-alive connections
# into it and will emit noise if Verdaccio disappears first.
hermetic_stop() {
	_hermetic_stop_proxy
	_hermetic_stop_verdaccio
}

# Swap Verdaccio to the warm (uplink-enabled) config. Used by bench.sh
# to bracket the per-tool populate step so any package the warm pass
# missed gets pulled from npmjs and cached locally — without this,
# subtle resolution differences (e.g. deno's warm fixture includes
# `is-odd` while the bench fixture does not) leave 404-holes that
# crash the cold-config populate. The throttle proxy stays up; client
# requests resume against the restarted Verdaccio on the same port.
hermetic_use_warm_uplink() {
	_hermetic_stop_verdaccio
	_hermetic_start_verdaccio "$HERMETIC_CONFIG_WARM"
}

# Swap Verdaccio back to the cold (no-uplink) config. Called after
# the populate step so the timed benchmark scenarios run against a
# fully hermetic registry — no npmjs jitter in the numbers.
hermetic_use_no_uplink() {
	_hermetic_stop_verdaccio
	_hermetic_start_verdaccio "$HERMETIC_CONFIG_COLD"
}
