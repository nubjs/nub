#!/usr/bin/env bash
# Shared benchmark provenance helpers, sourced by run.sh / run-warm-gvs.sh /
# cold-cas.sh.
#
# A committed results JSON is un-auditable without a record of what produced it:
# a "bun" timing block with no version is indistinguishable from a stale bun
# silently installing the wrong project. These helpers stamp every results file
# with a top-level `env` object — tool versions (keyed by tool, so a new tool
# slots in without a schema change), platform, arch, UTC timestamp, and the
# 1-min load average at measurement time — while leaving the per-tool timing
# blocks untouched (backward-compatible: a reader that ignores `env` is unaffected).

# bench_env_json <tool>=<version> [<tool>=<version> ...]
# Emit a compact JSON `env` object. Pass only the tools that actually ran; a tool
# whose version is empty or "(not installed)" is dropped, so `versions` lists
# exactly what measured the numbers.
bench_env_json() {
  local versions="" sep="" kv k v
  for kv in "$@"; do
    k="${kv%%=*}"; v="${kv#*=}"
    [[ -z "$v" || "$v" == "(not installed)" ]] && continue
    v="${v//\\/\\\\}"; v="${v//\"/\\\"}"   # JSON-escape \ and "
    versions+="${sep}\"${k}\":\"${v}\""
    sep=","
  done
  local platform arch ts load
  platform="$(uname -s | tr '[:upper:]' '[:lower:]')"   # darwin | linux
  arch="$(uname -m)"                                     # arm64 | x86_64 | aarch64
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  # First (1-min) load average, portable across macOS ("load averages: a b c")
  # and Linux ("load average: a, b, c") uptime formats.
  load="$(uptime | sed -E 's/.*load average[s]?:?[[:space:]]*//' | tr ',' ' ' | awk '{print $1}')"
  printf '{"platform":"%s","arch":"%s","timestamp_utc":"%s","load_1min":"%s","versions":{%s}}' \
    "$platform" "$arch" "$ts" "$load" "$versions"
}

# bench_inject_env <hyperfine_json_file> <env_json>
# Prepend "env":<env_json>, as the first key of a hyperfine --export-json file.
# Dependency-free: hyperfine writes one top-level object opening with `{`, so we
# rewrite that first byte to `{"env":<...>,`.
bench_inject_env() {
  local file="$1" env_json="$2" tmp
  [[ -f "$file" ]] || { echo "WARN: bench_inject_env: no file $file" >&2; return 0; }
  tmp="$(mktemp)"
  printf '{"env":%s,' "$env_json" > "$tmp"
  tail -c +2 "$file" >> "$tmp"
  mv "$tmp" "$file"
}
