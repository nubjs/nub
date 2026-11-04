#!/usr/bin/env bash
# Profile-Guided Optimization build for aube, with optional BOLT
# post-link rewrite.
#
# Three-phase rustc PGO flow, plus an optional BOLT step:
#
#   1. Build aube with -Cprofile-generate (instrumented binary).
#   2. Train against the committed offline registry — cold and warm
#      installs plus representative developer-loop commands against
#      pgo.package.json — so the profile covers resolver / registry /
#      store / linker hot paths, the frozen fast path, Node-backed script
#      dispatch, dependency inspection, and add/remove.
#   3. Merge .profraw via llvm-profdata, recompile with -Cprofile-use.
#   4. (AUBE_PGO_BOLT=1) re-link phase 3b with `--emit-relocs`, build
#      an instrumented variant via `llvm-bolt --instrument`, replay
#      the phase 2 training workload to collect per-process fdata,
#      `merge-fdata` them, then run `llvm-bolt` again to reorder
#      blocks + split cold paths using the merged profile.
#      Layered after PGO because LLVM's PGO and BOLT optimize different
#      things — PGO drives instruction-level codegen (inlining,
#      branch weights), BOLT does post-link block/function layout
#      and cold-path splitting that LLVM can't see at IR time.
#      Instrumentation rather than perf-LBR sampling so the flow
#      works on aarch64 (no LBR/BRBE dependency) and on hosts where
#      `kernel.perf_event_paranoid` denies branch-stack sampling.
#
# Local default: target/release-pgo/aube using profile=release-pgo.
#
# Holds /tmp/aube-bench.lock for the entire run because the offline
# training registry uses a shared port across worktrees and agents.
#
# CI hooks (env vars):
#   AUBE_PGO_NO_LOCK=1          skip /tmp/aube-bench.lock acquisition
#                               (also auto-skipped if `flock` is missing,
#                               e.g. on macOS).
#   AUBE_PGO_PROFILE=<profile>  cargo profile for both phases (default:
#                               release-pgo). Set to `release` in CI when
#                               the final build is delegated to another
#                               step.
#   AUBE_PGO_TARGET=<triple>    cross-compilation target (default: host).
#                               Output lands at target/<triple>/<profile>/.
#   AUBE_PGO_BUILD_TOOL=<tool>  `cargo` (default) or `cross`. cross is
#                               used in CI for Linux GNU/musl targets so
#                               the resulting binary keeps cross's older
#                               glibc baseline. Cross.toml passes RUSTFLAGS
#                               through to the container.
#   AUBE_PGO_USE_LLD=1          link both PGO build phases with clang + LLD.
#                               Uses the LLVM installation selected below.
#   AUBE_PGO_LLVM_VERSION=<n>   require /usr/lib/llvm-<n>/bin. Release CI
#                               pins this for reproducibility; local builds
#                               may omit it to auto-discover the newest
#                               complete toolchain, then PATH as a fallback.
#   RUSTFLAGS=<flags>           caller-provided flags are preserved and
#                               prepended to the phase-specific PGO flags.
#   AUBE_PGO_SKIP_FINAL_BUILD=1 stop after merging .profraw. Use when the
#                               final optimized build is delegated to a
#                               separate step (e.g. taiki-e action) that
#                               picks up RUSTFLAGS+CARGO_PROFILE_RELEASE_LTO
#                               from the environment.
#   AUBE_PGO_BOLT=1             append a BOLT post-link rewrite pass
#                               after phase 3b. Requires `llvm-bolt`
#                               and `merge-fdata` from a `bolt-NN`
#                               package — either on PATH or installed
#                               at /usr/lib/llvm-NN/bin/. Linux-only;
#                               BOLT's macOS support is not in tree.
#                               Uses BOLT's instrumentation mode (no
#                               `perf` needed), which works without
#                               LBR/BRBE branch sampling and without
#                               privileged kernel knobs.
#   AUBE_PGO_WORKFLOW_RUNS=<n>  repetitions for each warm developer-loop
#                               workflow (default: 20). Set to 0 for an
#                               install-only comparison build.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PGO_DATA_DIR="$REPO_ROOT/target/pgo-data"
PGO_PROFRAW_DIR="$PGO_DATA_DIR/profraw"
PGO_MERGED="$PGO_DATA_DIR/merged.profdata"

PGO_PROFILE="${AUBE_PGO_PROFILE:-release-pgo}"
PGO_TARGET="${AUBE_PGO_TARGET:-}"
PGO_BUILD_TOOL="${AUBE_PGO_BUILD_TOOL:-cargo}"
PGO_BOLT="${AUBE_PGO_BOLT:-}"
PGO_WORKFLOW_RUNS="${AUBE_PGO_WORKFLOW_RUNS:-20}"
PGO_REGISTRY_PORT="${AUBE_PGO_REGISTRY_PORT:-4876}"
PGO_REGISTRY_URL="http://127.0.0.1:$PGO_REGISTRY_PORT"
PGO_NODE_BIN="$(node -p 'process.execPath')"
PGO_NODE_DIR="$(dirname "$PGO_NODE_BIN")"
PGO_TRAIN_PATH="$PGO_NODE_DIR:$PATH"
BASE_RUSTFLAGS="${RUSTFLAGS:-}"

find_llvm_prefix() {
	local required_tools=""
	local prefix tool complete tool_path

	if [ -n "${AUBE_PGO_USE_LLD:-}" ]; then
		required_tools="clang ld.lld"
	fi
	if [ -n "$PGO_BOLT" ]; then
		required_tools="${required_tools:+$required_tools }llvm-bolt merge-fdata"
	fi

	prefix="${AUBE_PGO_LLVM_VERSION:+/usr/lib/llvm-$AUBE_PGO_LLVM_VERSION}"
	if [ -n "$prefix" ]; then
		for tool in $required_tools; do
			if [ ! -x "$prefix/bin/$tool" ]; then
				echo "ERROR: LLVM prefix $prefix is missing $tool" >&2
				exit 1
			fi
		done
		printf '%s\n' "$prefix"
		return 0
	fi

	while IFS= read -r prefix; do
		complete=1
		for tool in $required_tools; do
			if [ ! -x "$prefix/bin/$tool" ]; then
				complete=0
				break
			fi
		done
		if [ "$complete" -eq 1 ]; then
			printf '%s\n' "$prefix"
			return 0
		fi
	done < <(find /usr/lib -maxdepth 1 -type d -name 'llvm-[0-9]*' 2>/dev/null | sort -V -r)

	for tool in $required_tools; do
		tool_path=$(command -v "$tool" || true)
		if [ -z "$tool_path" ] || [ ! -x "$tool_path" ]; then
			echo "ERROR: no installed LLVM toolchain found" >&2
			exit 1
		fi
	done
	printf '%s\n' PATH
}

LLVM_PREFIX=""
LLVM_BINDIR=""
if [ -n "${AUBE_PGO_USE_LLD:-}" ] || [ -n "$PGO_BOLT" ]; then
	LLVM_PREFIX="$(find_llvm_prefix)"
	if [ "$LLVM_PREFIX" = PATH ]; then
		echo ">>> LLVM toolchain: PATH"
	else
		LLVM_BINDIR="$LLVM_PREFIX/bin"
		export PATH="$LLVM_BINDIR:$PATH"
		echo ">>> LLVM toolchain: $LLVM_PREFIX"
	fi

	if [ -n "${AUBE_PGO_USE_LLD:-}" ]; then
		if [ -n "$LLVM_BINDIR" ]; then
			PGO_CLANG="$LLVM_BINDIR/clang"
			PGO_LLD="$LLVM_BINDIR/ld.lld"
		else
			PGO_CLANG="$(command -v clang)"
			PGO_LLD="$(command -v ld.lld)"
		fi
		BASE_RUSTFLAGS="${BASE_RUSTFLAGS:+$BASE_RUSTFLAGS }-C linker=$PGO_CLANG -C link-arg=-fuse-ld=lld"
		echo ">>> PGO linker: $PGO_CLANG + $PGO_LLD"
	fi
fi

case "$PGO_WORKFLOW_RUNS" in
'' | *[!0-9]*)
	echo "ERROR: AUBE_PGO_WORKFLOW_RUNS must be a non-negative integer" >&2
	exit 1
	;;
esac

case "$PGO_REGISTRY_PORT" in
'' | *[!0-9]*)
	echo "ERROR: AUBE_PGO_REGISTRY_PORT must be an integer" >&2
	exit 1
	;;
esac

if [ -n "$PGO_BOLT" ]; then
	# Keep BOLT and merge-fdata on the same versioned LLVM installation
	# selected above. Using the versioned prefix also preserves BOLT's
	# runtime lookup for /usr/lib/llvm-NN/lib/libbolt_rt_instr.a.
	if [ -n "$LLVM_BINDIR" ]; then
		LLVM_BOLT="$LLVM_BINDIR/llvm-bolt"
		MERGE_FDATA="$LLVM_BINDIR/merge-fdata"
	else
		LLVM_BOLT="$(command -v llvm-bolt)"
		MERGE_FDATA="$(command -v merge-fdata)"
	fi
	echo ">>> BOLT toolchain: $LLVM_BOLT ($MERGE_FDATA)"
fi

# target_arg stays unquoted at expansion sites: empty string disappears,
# "--target=foo" expands to one arg. Avoids bash 3.2 (macOS) array+set -u
# unbound-variable issues with "${arr[@]}".
target_arg=""
target_dir_part=""
if [ -n "$PGO_TARGET" ]; then
	target_arg="--target=$PGO_TARGET"
	target_dir_part="$PGO_TARGET/"
fi

if [ -z "${AUBE_PGO_NO_LOCK:-}" ] && command -v flock >/dev/null 2>&1; then
	echo ">>> Acquiring /tmp/aube-bench.lock (30 min timeout)"
	exec 9>/tmp/aube-bench.lock
	if ! flock -w 1800 9; then
		echo "ERROR: failed to acquire /tmp/aube-bench.lock after 30 min" >&2
		exit 1
	fi
	echo ">>> Lock acquired"
else
	echo ">>> Skipping /tmp/aube-bench.lock (AUBE_PGO_NO_LOCK or flock missing)"
fi

training_registry_start() {
	if curl -fsS "$PGO_REGISTRY_URL/" >/dev/null 2>&1; then
		echo "ERROR: port $PGO_REGISTRY_PORT is already in use" >&2
		exit 1
	fi

	"$PGO_NODE_BIN" "$SCRIPT_DIR/offline-registry.mts" --port "$PGO_REGISTRY_PORT" \
		>"$train_dir/offline-registry.log" 2>&1 &
	PGO_REGISTRY_PID=$!

	local retries=60
	while ! curl -fsS "$PGO_REGISTRY_URL/" >/dev/null 2>&1; do
		if ! kill -0 "$PGO_REGISTRY_PID" 2>/dev/null; then
			echo "ERROR: offline training registry exited during startup" >&2
			cat "$train_dir/offline-registry.log" >&2
			exit 1
		fi
		retries=$((retries - 1))
		if [ "$retries" -le 0 ]; then
			echo "ERROR: offline training registry failed to start" >&2
			exit 1
		fi
		sleep 0.1
	done
	echo "Offline training registry: $PGO_REGISTRY_URL"
}

training_registry_stop() {
	if [ -n "${PGO_REGISTRY_PID:-}" ]; then
		kill "$PGO_REGISTRY_PID" 2>/dev/null || true
		wait "$PGO_REGISTRY_PID" 2>/dev/null || true
		unset PGO_REGISTRY_PID
	fi
}

RUSTC_HOST="$(rustc -vV | sed -n 's|^host: ||p')"
RUSTC_SYSROOT="$(rustc --print sysroot)"
LLVM_PROFDATA="$RUSTC_SYSROOT/lib/rustlib/$RUSTC_HOST/bin/llvm-profdata"
if [ ! -x "$LLVM_PROFDATA" ]; then
	echo "ERROR: llvm-profdata not found at $LLVM_PROFDATA" >&2
	echo "  Install with: rustup component add llvm-tools-preview" >&2
	exit 1
fi

mkdir -p "$PGO_PROFRAW_DIR"
rm -f "$PGO_PROFRAW_DIR"/*.profraw "$PGO_MERGED"

# With AUBE_PGO_BUILD_TOOL=cross, rustc runs inside a container that
# mounts the project at `/project` (not at the host path), so when
# phase 3b reads `-Cprofile-use=<host-path>` from RUSTFLAGS the file
# is invisible — rustc bails with "file ... does not exist" even when
# the merge step wrote it on the host. Bind-mount PGO_DATA_DIR at the
# same host path inside the container so the existing RUSTFLAGS value
# resolves. Harmless on the host-side phase 1 build (cross still
# writes the instrumented binary to target/ via its own bind mount).
if [ "$PGO_BUILD_TOOL" = "cross" ]; then
	export CROSS_CONTAINER_OPTS="${CROSS_CONTAINER_OPTS:-} -v $PGO_DATA_DIR:$PGO_DATA_DIR:rw"
fi

# ---------- Phase 1: instrumented build ----------
echo ">>> [1/3] Building instrumented binary ($PGO_BUILD_TOOL, profile=$PGO_PROFILE${PGO_TARGET:+, target=$PGO_TARGET})"
# shellcheck disable=SC2086 # intentional word-splitting on $target_arg
RUSTFLAGS="${BASE_RUSTFLAGS:+$BASE_RUSTFLAGS }-Cprofile-generate=$PGO_PROFRAW_DIR" \
	"$PGO_BUILD_TOOL" build --profile="$PGO_PROFILE" $target_arg -p aube --bin aube

INSTRUMENTED_BIN="$REPO_ROOT/target/${target_dir_part}${PGO_PROFILE}/aube"
if [ ! -x "$INSTRUMENTED_BIN" ]; then
	echo "ERROR: instrumented binary missing at $INSTRUMENTED_BIN" >&2
	exit 1
fi

# ---------- Phase 2: training ----------
echo ">>> [2/3] Training against committed offline registry"

train_dir="$(mktemp -d "${TMPDIR:-/tmp}/aube-pgo-train.XXXXXX")"
cleanup() {
	training_registry_stop
	rm -rf "$train_dir"
}
trap cleanup EXIT

training_registry_start

# Force the instrumented binary to write profraw to a host path we
# control, regardless of what `-Cprofile-generate=<dir>` baked in at
# compile time. With AUBE_PGO_BUILD_TOOL=cross the rustc compile runs
# inside a container where the project may be mounted under a path
# that differs from the host (cross's default MOUNT_FINDER puts the
# workspace at `/project`). The host path embedded in the binary then
# doesn't resolve at runtime, profraw goes nowhere, and llvm-profdata
# silently produces no merged output. Setting LLVM_PROFILE_FILE at
# runtime sidesteps the path-translation question entirely. %m
# disambiguates per module signature; %p per process — together they
# keep the install and developer-workflow runs from colliding on the same file.
PROFRAW_PATTERN="$PGO_PROFRAW_DIR/aube-%m-%p.profraw"

# 3 cold + 3 warm installs, followed by representative warm workflows.
# Cold runs each get a fresh dir so the resolver, registry, store, and
# linker hot paths all run end-to-end. Warm runs reuse the last cold dir
# so the frozen-lockfile fast path is also represented in the profile.
# The workflow pass runs a real installed Node binary via both aubr and
# exec, reads the dependency graph via list/why, then removes and re-adds
# a dependency. The binary is parameterized so
# phase 4 (BOLT) can replay the same workload against the BOLT-instrumented
# variant of the PGO-optimized binary.
cold_run() {
	local bin=$1 i=$2 tag=${3:-cold}
	local run_dir="$train_dir/$tag.$i"
	rm -rf "$run_dir"
	mkdir -p "$run_dir/home"
	cp "$SCRIPT_DIR/pgo.package.json" "$run_dir/package.json"
	{
		printf 'registry=%s\n' "$PGO_REGISTRY_URL"
		printf 'advisoryCheck=off\n'
		printf 'advisoryCheckOnInstall=off\n'
		printf 'lowDownloadThreshold=0\n'
	} >"$run_dir/.npmrc"
	cp "$run_dir/.npmrc" "$run_dir/home/.npmrc"
	echo "  train: cold install ($i)"
	(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$bin" install --ignore-scripts >/dev/null)
}

warm_run() {
	local bin=$1 run_dir=$2 i=$3
	echo "  train: warm install ($i)"
	(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$bin" install --ignore-scripts >/dev/null)
}

train_workflows() {
	local bin=$1 run_dir=$2
	local shim_dir="$run_dir/bin"
	local i

	if [ "$PGO_WORKFLOW_RUNS" -eq 0 ]; then
		echo "  train: developer workflows skipped (AUBE_PGO_WORKFLOW_RUNS=0)"
		return
	fi

	mkdir -p "$shim_dir"
	ln -sf "$bin" "$shim_dir/aubr"

	echo "  train: developer workflows ($PGO_WORKFLOW_RUNS repetitions each)"
	i=1
	while [ "$i" -le "$PGO_WORKFLOW_RUNS" ]; do
		(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$shim_dir/aubr" check-version >/dev/null)
		(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$bin" exec --no-install semver 1.2.3 >/dev/null)
		(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$bin" list --depth 1 >/dev/null)
		(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$bin" why is-number --parseable >/dev/null)
		i=$((i + 1))
	done

	echo "  train: remove + add"
	(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$bin" remove undici --ignore-scripts >/dev/null)
	(cd "$run_dir" && HOME="$run_dir/home" PATH="$PGO_TRAIN_PATH" "$bin" add undici@6.25.0 --ignore-scripts --allow-low-downloads >/dev/null)
}

# LLVM_PROFILE_FILE only matters for the instrumented binary, so it's
# exported just around phase 2's training loop.
export LLVM_PROFILE_FILE="$PROFRAW_PATTERN"
for i in 1 2 3; do
	cold_run "$INSTRUMENTED_BIN" "$i" cold
done
for i in 1 2 3; do
	warm_run "$INSTRUMENTED_BIN" "$train_dir/cold.3" "$i"
done
train_workflows "$INSTRUMENTED_BIN" "$train_dir/cold.3"
unset LLVM_PROFILE_FILE

training_registry_stop

# Sanity check: confirm training actually wrote profraw. Without
# LLVM_PROFILE_FILE this silently produced zero files on cross-built
# targets — llvm-profdata then merged nothing and phase 3b failed with
# "file ... merged.profdata does not exist". Fail loudly here instead.
profraw_count=$(find "$PGO_PROFRAW_DIR" -maxdepth 1 -name '*.profraw' -type f | wc -l | tr -d ' ')
if [ "$profraw_count" -eq 0 ]; then
	echo "ERROR: no .profraw files written to $PGO_PROFRAW_DIR after training" >&2
	echo "  Training ran but the instrumented binary did not record profile data." >&2
	echo "  Check LLVM_PROFILE_FILE handling and the cross host/container mount." >&2
	exit 1
fi
echo ">>> $profraw_count .profraw files collected"

# ---------- Phase 3a: merge ----------
echo ">>> [3/3] Merging profile data"
"$LLVM_PROFDATA" merge -o "$PGO_MERGED" "$PGO_PROFRAW_DIR"

# Defense in depth: confirm llvm-profdata actually wrote the merged
# file. A version mismatch between the rustc that instrumented (phase 1
# inside cross) and the host's llvm-profdata can produce a 0-exit
# silent no-op. Without this check we'd fall through to phase 3b and
# see rustc's opaque "file ... does not exist" against a path that
# really doesn't exist on the host at all.
if [ ! -f "$PGO_MERGED" ]; then
	echo "ERROR: $PGO_MERGED was not produced by llvm-profdata merge" >&2
	echo "  Check that the host's llvm-profdata version matches the rustc that built the instrumented binary." >&2
	exit 1
fi
echo ">>> merged profile written: $(stat -c %s "$PGO_MERGED" 2>/dev/null || stat -f %z "$PGO_MERGED") bytes"

if [ -n "${AUBE_PGO_SKIP_FINAL_BUILD:-}" ]; then
	echo ">>> Skipping final optimized build (AUBE_PGO_SKIP_FINAL_BUILD=1)"
	echo ">>> Profile ready at: $PGO_MERGED"
	exit 0
fi

# ---------- Phase 3b: optimize ----------
echo ">>> Rebuilding with -Cprofile-use${PGO_BOLT:+ + --emit-relocs}"

# -Cllvm-args=-pgo-warn-missing-function=false: silence LLVM's per-symbol
# "no profile data available for function …" notes during phase 3b.
# Coverage gaps are expected — the training run can't exercise every
# code path — and emitting a warning per uncovered symbol drowns the CI
# build log without surfacing actionable signal. The functions still
# get compiled, just without PGO data, which is the documented fallback.
#
# When AUBE_PGO_BOLT=1, add `--emit-relocs` so the binary keeps its
# `.rela.text` table after link. BOLT needs the relocations to rewrite
# branch targets when it moves blocks around; without them it falls
# back to a much less effective mode that only reorders within
# functions.
phase3b_rustflags="${BASE_RUSTFLAGS:+$BASE_RUSTFLAGS }-Cprofile-use=$PGO_MERGED -Cllvm-args=-pgo-warn-missing-function=false"
if [ -n "$PGO_BOLT" ]; then
	phase3b_rustflags="$phase3b_rustflags -C link-arg=-Wl,--emit-relocs -C link-arg=-Wl,-q"
fi
# shellcheck disable=SC2086 # intentional word-splitting on $target_arg
RUSTFLAGS="$phase3b_rustflags" \
	"$PGO_BUILD_TOOL" build --profile="$PGO_PROFILE" $target_arg -p aube --bin aube

# Phase 3b wrote to the same path as phase 1, so the file at
# $INSTRUMENTED_BIN is now the PGO-optimized build, not the instrumented
# one. Alias for clarity in the success log.
PGO_FINAL_BIN="$INSTRUMENTED_BIN"
echo ">>> PGO build complete: $PGO_FINAL_BIN"
ls -lh "$PGO_FINAL_BIN"

if [ -z "$PGO_BOLT" ]; then
	exit 0
fi

# ---------- Phase 4: BOLT post-link rewrite (instrumentation mode) ----------
# Why instrumentation rather than the more common `perf record + perf2bolt`
# LBR flow:
#   - `perf record -j any,u` needs `kernel.perf_event_paranoid <= 1`, which
#     the CI runners used for PGO don't guarantee or let workflows reconfigure.
#   - aarch64 hosts without ARM v9.2 BRBE can't do LBR sampling at all,
#     so the perf flow would silently miss profile data on the
#     aarch64-linux PGO row even if paranoid were 0.
# Instrumentation sidesteps both: BOLT injects counters into the binary,
# the instrumented binary writes one fdata file per process at exit,
# and `merge-fdata` rolls them up into a single profile. Slower training
# (instrumented binary is ~5× slower than native) but the workload is
# small enough that the wall cost is well under a minute.
echo ">>> [4/4] BOLT post-link rewrite (instrumentation mode)"

BOLT_INSTR_BIN="$PGO_DATA_DIR/aube.instr"
BOLT_FDATA_PREFIX="$PGO_DATA_DIR/bolt"
BOLT_FDATA="$PGO_DATA_DIR/aube.fdata"
rm -f "$BOLT_INSTR_BIN" "$BOLT_FDATA"
find "$PGO_DATA_DIR" -maxdepth 1 -name 'bolt.*.fdata' -delete 2>/dev/null || true

echo ">>> [4a/4] Building instrumented binary"
"$LLVM_BOLT" "$PGO_FINAL_BIN" \
	--instrument \
	--instrumentation-file="$BOLT_FDATA_PREFIX" \
	--instrumentation-file-append-pid \
	-o "$BOLT_INSTR_BIN"

# Replay the same install + developer workflow mix as phase 2, this time
# against the instrumented PGO binary. Each invocation writes
# bolt.<pid>.fdata on exit. The offline registry was stopped at end of
# phase 2; restart it here.
training_registry_start

echo ">>> [4b/4] Training instrumented binary"
for i in 1 2 3; do cold_run "$BOLT_INSTR_BIN" "$i" bolt; done
for i in 1 2 3; do warm_run "$BOLT_INSTR_BIN" "$train_dir/bolt.3" "$i"; done
train_workflows "$BOLT_INSTR_BIN" "$train_dir/bolt.3"

training_registry_stop

# Sanity check: instrumentation writes on `_exit`. If the binary
# crashed mid-run, no fdata. Without this we'd fall through to
# llvm-bolt with an empty profile.
fdata_files=("$PGO_DATA_DIR"/bolt.*.fdata)
if [ ! -e "${fdata_files[0]}" ]; then
	echo "ERROR: no bolt.*.fdata files written to $PGO_DATA_DIR" >&2
	echo "  Instrumented binary may have crashed before exit." >&2
	exit 1
fi
echo ">>> ${#fdata_files[@]} fdata files collected"

echo ">>> [4c/4] Merging fdata"
"$MERGE_FDATA" "${fdata_files[@]}" -o "$BOLT_FDATA"

# llvm-bolt flags:
#   reorder-blocks=ext-tsp     — extended TSP block layout, the
#                                strongest available.
#   reorder-functions=cdsort   — call-density sort. Hot functions
#                                cluster so the kernel maps them
#                                out of the same pages.
#   split-functions            — split each function into hot/cold so
#                                the cold parts don't waste i-cache.
#   split-all-cold             — aggressive: split even functions
#                                BOLT isn't 100% sure about.
#   split-eh                   — split exception-handling paths;
#                                aube's hot install path raises ~zero.
#   use-gnu-stack              — emit a PT_GNU_STACK header so the
#                                kernel keeps the stack non-executable.
echo ">>> [4d/4] Rewriting binary"
"$LLVM_BOLT" "$PGO_FINAL_BIN" \
	-o "$PGO_FINAL_BIN.bolt" \
	-data="$BOLT_FDATA" \
	-reorder-blocks=ext-tsp \
	-reorder-functions=cdsort \
	-split-functions \
	-split-all-cold \
	-split-eh \
	-use-gnu-stack

mv -f "$PGO_FINAL_BIN.bolt" "$PGO_FINAL_BIN"
echo ">>> PGO+BOLT build complete: $PGO_FINAL_BIN"
ls -lh "$PGO_FINAL_BIN"
