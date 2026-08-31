#!/usr/bin/env sh
# target-gc — self-pruning for nub's Rust build residue.
#
# The two families that fill the disk (measured 2026-08-14: 101 GiB + 87 GiB
# on a volume down to 340 MiB free) are worktree-private target/ dirs and
# orphaned content-keyed shared buckets under ~/.cache/nub. Both used to wait
# for a human to run the audit scripts; this collector runs from rust-build.sh
# after target resolution, at most hourly, so the residue prunes itself the
# way mbx (mr boxington) prunes its managed target directories: abandoned
# state is collected unconditionally, idle state by age, and a low-disk
# pressure pass collects harder while sparing the directory most recently
# built in — deleting that one cannot hold the total down, because the next
# build recreates it.
#
# WHAT IT COLLECTS, AND WHEN:
#   - a BUCKET (shared-target-<key>) no live worktree resolves to, untouched
#     for NUB_GC_ORPHAN_HOURS (24): the key moved with trunk, nothing can join
#     it again. Resolution is asked of each worktree's OWN rust-build.sh
#     --print-target — a pure read — so there is no duplicated key computation
#     to drift, which is the failure the manual sweep's positive control
#     exists to catch.
#   - any bucket or live worktree's target/ untouched for NUB_GC_AGE_DAYS
#     (14): the incumbent age rule, unchanged.
#   - under PRESSURE (free space below NUB_GC_FREE_MIN_GIB, default 100):
#     orphan buckets at any age, and idle directories beyond
#     NUB_GC_PRESSURE_HOURS (24) oldest-first — sparing the newest non-orphan
#     that would otherwise go, so an all-idle fleet is never wiped at once.
#   - .seeding claims abandoned for 2h, and tombs a killed collector left.
#
# WHAT IT NEVER COLLECTS:
#   - anything in NUB_GC_KEEP (colon-separated; the calling build passes its
#     own CARGO_TARGET_DIR);
#   - anything whose top three levels saw a write in the last 2h — the in-use
#     signal for a build in flight (a wrapper `touch` marks the top level at
#     every build start, and artifacts landing keep the profile dirs fresh);
#   - the newest rlib-bearing bucket (the seed every isolated worktree clones);
#   - the directory the installed nub-dev symlink resolves into — a shared
#     bucket or an isolated worktree's target/ alike (deleting it breaks the
#     dev binary with no error until it is run).
#
# Deletion is tomb-based: `mv` aside, then rm -rf. A build racing the
# collector into the same directory then recreates it fresh via mkdir -p and
# pays a seeded rebuild — never reads a half-deleted tree. That is the same
# accepted race the bucket sweep always had, narrowed from the width of an
# rm -rf to the width of a rename.
#
# Fail-open everywhere: any read or delete that fails skips that entry; a
# collector that cannot take the lock exits 0 silently. NUB_TARGET_GC=0
# disables collection entirely.
#
#   scripts/target-gc.sh             # collect (rust-build.sh calls this hourly)
#   scripts/target-gc.sh --dry-run   # print what would be collected, delete nothing
#
# Tunables: NUB_GC_ORPHAN_HOURS (24), NUB_GC_AGE_DAYS (14, 0 disables the age
# rule), NUB_GC_FREE_MIN_GIB (100, 0 disables the pressure pass),
# NUB_GC_PRESSURE_HOURS (24), NUB_GC_KEEP, NUB_GC_CACHE_ROOT (test override),
# NUB_TARGET_GC=0.
set -u

[ "${NUB_TARGET_GC:-1}" = "0" ] && exit 0

# An inherited NUB_SHARED_TARGET would poison every resolution probe below:
# the wrapper honours it verbatim, so all N worktrees would print one
# identical path, no real bucket would land in the resolved set, and — the
# set being non-empty — the empty-set valve would not fire, classifying every
# live bucket an orphan. make verify/fmt/addon all export it, and the hourly
# hook runs inside exactly those builds. Unset covers this process AND the
# child probes.
unset NUB_SHARED_TARGET

dry=""
[ "${1:-}" = "--dry-run" ] && dry=1

cache=${NUB_GC_CACHE_ROOT:-$HOME/.cache/nub}
shared="$cache/shared-target"
orphan_hours=${NUB_GC_ORPHAN_HOURS:-24}
age_days=${NUB_GC_AGE_DAYS:-14}
free_min=${NUB_GC_FREE_MIN_GIB:-100}
pressure_hours=${NUB_GC_PRESSURE_HOURS:-24}
# A non-numeric tunable would turn every comparison below into a shell error;
# fall back to the default instead (the governor's rule).
[ "$orphan_hours" -ge 0 ] 2>/dev/null || orphan_hours=24
[ "$age_days" -ge 0 ] 2>/dev/null || age_days=14
[ "$free_min" -ge 0 ] 2>/dev/null || free_min=100
[ "$pressure_hours" -ge 1 ] 2>/dev/null || pressure_hours=24

# One collector at a time; a lock older than 30 min belonged to a killed one.
lock="$cache/target-gc.lock"
if [ -d "$lock" ] && [ -z "$(find "$lock" -maxdepth 0 -mmin -30 2>/dev/null)" ]; then
  rm -rf "$lock" 2>/dev/null
fi
mkdir -p "$cache" 2>/dev/null || exit 0
mkdir "$lock" 2>/dev/null || exit 0
trap 'rm -rf "$lock" 2>/dev/null' EXIT

say() { printf 'target-gc: %s\n' "$*" >&2; }

# Every path is compared as a string, so every path that exists is reduced to
# its one physical spelling first — a symlinked component anywhere (macOS's
# /var -> /private/var, a symlinked $HOME) would otherwise make the keep and
# resolved sets silently miss the candidates they name. A path that does not
# exist yet (a bucket --print-target names but nothing created) passes through
# raw: it cannot be a deletion candidate, so a mismatch there is harmless.
# shellcheck disable=SC1007  # CDPATH= is a deliberate empty assignment for this one command
canon() { CDPATH= cd -P -- "$1" 2>/dev/null && pwd -P || printf '%s\n' "$1"; }
cache=$(canon "$cache")
shared="$cache/shared-target"

nl='
'

# ── The protect set ─────────────────────────────────────────────────────────
# keep: newline-delimited absolute paths that survive every pass. Matched
# whole-line (each entry sits between newlines), so /a/b never shields /x/a/b.
keep=$nl
old_ifs=$IFS; IFS=:
for _k in ${NUB_GC_KEEP:-}; do [ -n "$_k" ] && keep="$keep$(canon "$_k")$nl"; done
IFS=$old_ifs

# The directory nub-dev resolves into: the symlink points at <dir>/fast/nub,
# where <dir> is a shared bucket OR an isolated worktree's target/ — whichever
# --print-target answered when make install-dev last ran. Both shapes are
# collectible, so both are kept; no path filter, because narrowing this to the
# cache root once left the isolated shape unprotected.
for _dev in nub-dev nubx-dev; do
  _bin=$(command -v "$_dev" 2>/dev/null) || continue
  _dst=$(readlink "$_bin" 2>/dev/null) || continue
  _dir=${_dst%/*/*}
  [ -n "$_dir" ] && [ -d "$_dir" ] && keep="$keep$(canon "$_dir")$nl"
done

# The newest rlib-bearing bucket — the seed every isolated worktree clones.
# shellcheck disable=SC2010,SC2012  # names are ours and contain no newlines
for _b in $(ls -dt "$shared"-* 2>/dev/null | grep -v '\.seeding$' | grep -v '\.gc\.'); do
  if [ -n "$(find "$_b" -name '*.rlib' -print -quit 2>/dev/null)" ]; then
    keep="$keep$_b$nl"
    break
  fi
done

# Live worktrees, and the target dir each one would build into today. Asking
# each checkout's own wrapper keeps this collector free of any second copy of
# the content-key computation. A worktree whose wrapper predates --print-target
# or fails contributes nothing — its bucket is then only protected by age and
# the in-use probe, which is the fail-open direction (one reseed, not a wedge).
roots=$(git worktree list --porcelain 2>/dev/null | awk '/^worktree /{sub(/^worktree /, ""); print}' | head -40)
resolved=$nl
while read -r _root; do
  [ -n "$_root" ] && [ -d "$_root" ] || continue
  if [ -f "$_root/scripts/rust-build.sh" ]; then
    _t=$(cd "$_root" 2>/dev/null && sh scripts/rust-build.sh --print-target 2>/dev/null) || _t=""
    [ -n "$_t" ] && resolved="$resolved$(canon "$_t")$nl"
  fi
done <<ROOTS
$roots
ROOTS

kept()     { case "$keep"     in *"$nl$1$nl"*) return 0 ;; esac; return 1; }
# With no worktree resolutions at all — not a git repo, or every wrapper failed
# — orphanhood is unknowable, so resolves() answers "yes" for every bucket and
# the orphan rule disables itself. Age, keep and in-use still apply.
resolves() {
  [ "$resolved" = "$nl" ] && return 0
  case "$resolved" in *"$nl$1$nl"*) return 0 ;; esac; return 1
}
# A write anywhere in the top three levels within 2h means a build may be in
# flight (top-level touch at build start; artifacts keep profile dirs fresh).
in_use() { [ -n "$(find "$1" -maxdepth 3 -mmin -120 -print -quit 2>/dev/null)" ]; }

collect() {
  if [ -n "$dry" ]; then
    say "would collect $1 ($2)"
    return 0
  fi
  _tomb="$1.gc.$$"
  if mv "$1" "$_tomb" 2>/dev/null; then
    rm -rf "$_tomb" 2>/dev/null
    say "collected $1 ($2)"
  fi
}

# ── Pressure detection ──────────────────────────────────────────────────────
pressure=""
if [ "${NUB_GC_PRESSURE:-}" = "1" ]; then
  pressure=1
elif [ "$free_min" -gt 0 ]; then
  _avail=$(df -Pk "$cache" 2>/dev/null | awk 'NR==2{print $4}')
  if [ "$_avail" -ge 0 ] 2>/dev/null && [ "$_avail" -lt $((free_min * 1024 * 1024)) ]; then
    pressure=1
    say "pressure: $((_avail / 1024 / 1024)) GiB free, floor ${free_min} GiB — collecting idle state"
  fi
fi

# ── Housekeeping: abandoned seeding claims and tombs ────────────────────────
[ -z "$dry" ] && find "$(dirname "$shared")" -maxdepth 1 -type d -name "$(basename "$shared")*.seeding" \
  -mmin +120 -exec rm -rf {} + 2>/dev/null
[ -z "$dry" ] && find "$(dirname "$shared")" -maxdepth 1 -type d -name "$(basename "$shared")*.gc.*" \
  -mmin +120 -exec rm -rf {} + 2>/dev/null

# ── Candidate scan ──────────────────────────────────────────────────────────
# Buckets (plus the bare legacy dir), then each live worktree's real target/.
# One candidate per line: "<epoch-mtime> <kind> <path>". Sorting survivors by
# mtime lets the pressure pass go oldest-first and spare the newest.
stat_mtime() { stat -f %m "$1" 2>/dev/null || stat -c %Y "$1" 2>/dev/null || echo 0; }

candidates=""
for _b in "$shared" "$shared"-*; do
  [ -d "$_b" ] || continue
  case $_b in *.seeding | *.gc.*) continue ;; esac
  candidates="$candidates$(stat_mtime "$_b") bucket $_b$nl"
done
while read -r _root; do
  [ -n "$_root" ] || continue
  _t="$_root/target"
  [ -d "$_t" ] && [ ! -L "$_t" ] || continue
  candidates="$candidates$(stat_mtime "$_t") target $_t$nl"
done <<ROOTS
$roots
ROOTS

now=$(date +%s 2>/dev/null) || now=""
[ "$now" -ge 1 ] 2>/dev/null || exit 0

# ── Normal pass ─────────────────────────────────────────────────────────────
survivors=""
while read -r _m _kind _path; do
  [ -n "$_path" ] || continue
  [ "$_m" -ge 1 ] 2>/dev/null || continue
  _h=$(( (now - _m) / 3600 ))
  kept "$_path" && continue
  in_use "$_path" && continue
  if [ "$age_days" -gt 0 ] && [ "$_h" -ge $((age_days * 24)) ]; then
    collect "$_path" "untouched ${age_days}d"
  elif [ "$_kind" = bucket ] && ! resolves "$_path" && [ "$_h" -ge "$orphan_hours" ]; then
    collect "$_path" "orphaned bucket, ${_h}h old"
  else
    survivors="$survivors$_m $_kind $_path$nl"
  fi
done <<CANDIDATES
$candidates
CANDIDATES

# ── Pressure pass ───────────────────────────────────────────────────────────
# Oldest-first over what the normal pass left. Orphan buckets go at any age.
# Everything else goes once idle past NUB_GC_PRESSURE_HOURS — resolved buckets
# included, since a rebuild recovers them — EXCEPT the newest directory that
# would otherwise be collected: when the whole fleet has sat idle (a weekend),
# that spare is what keeps pressure from wiping every warm dir at once, and
# it never goes to an orphan (which is being deleted regardless, so the
# protection would be spent on nothing — mbx's rule).
if [ -n "$pressure" ] && [ -n "$survivors" ]; then
  sorted=$(printf '%s' "$survivors" | sort -n)
  spare=""
  while read -r _m _kind _path; do
    [ -n "$_path" ] || continue
    [ "$_m" -ge 1 ] 2>/dev/null || continue
    _h=$(( (now - _m) / 3600 ))
    if [ "$_h" -ge "$pressure_hours" ] \
      && { [ "$_kind" = target ] || resolves "$_path"; }; then spare=$_path; fi
  done <<SORTED
$sorted
SORTED
  while read -r _m _kind _path; do
    [ -n "$_path" ] || continue
    [ "$_path" = "$spare" ] && continue
    _h=$(( (now - _m) / 3600 ))
    if [ "$_kind" = bucket ] && ! resolves "$_path"; then
      collect "$_path" "pressure: orphaned bucket"
    elif [ "$_h" -ge "$pressure_hours" ]; then
      collect "$_path" "pressure: idle ${_h}h"
    fi
  done <<SORTED
$sorted
SORTED
fi

exit 0
