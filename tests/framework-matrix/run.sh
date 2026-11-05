#!/usr/bin/env bash
# run.sh — one framework's out-of-the-box acceptance run for the fresh-build
# matrix. Given an already-scaffolded project, it exercises the THREE distinct
# code paths (dev server, production build, serve-the-build) and asserts each,
# plus verifies the symlink-GVS / eject linking layout is actually in use.
#
# Usage: run.sh <name> <proj-dir> <dev-cmd> <build-cmd> <preview-cmd> [dep-to-probe]
#   Commands run from inside <proj-dir>; "-" skips a step. Server commands must
#   bind 127.0.0.1 and print their URL to stdout/stderr (the port is parsed from
#   the log; pass --port/--host through where the framework needs it).
#   <dep-to-probe> is a node_modules dep whose realpath decides the layout verdict
#   (default: pick the first top-level dep). Set NUB=<path> to the nub binary.
#
# Emits one ROW| line per step plus a final VERDICT| line. Behavioral assertions
# (HTTP status, build exit code) are load-INDEPENDENT — no wall-clock gating.
set -u

NUB="${NUB:?set NUB to the nub binary path}"
name="$1"; proj="$2"; dev_cmd="$3"; build_cmd="$4"; preview_cmd="$5"; probe="${6:-}"

log() { echo "ROW|$name|$*"; }

# TERM then KILL each descendant — a dev-server worker that ignores SIGTERM must
# not survive to hold a port into the next stage. The group-kill in kill_group is
# the primary reaper; this PID-walk is the fallback for anything that escaped the
# group (e.g. via its own setsid), so it also needs the follow-up KILL.
kill_tree() { local p="$1" c; for c in $(pgrep -P "$p" 2>/dev/null); do kill_tree "$c"; done; kill -TERM "$p" 2>/dev/null; kill -KILL "$p" 2>/dev/null; }
kill_group() { local g="$1"; kill -TERM -"$g" 2>/dev/null; sleep 0.4; kill -KILL -"$g" 2>/dev/null; kill_tree "$g"; kill -KILL "$g" 2>/dev/null; }

cd "$proj" || { log "step=setup result=FAIL detail=no-dir"; echo "VERDICT|$name|FAIL"; exit 2; }

# ── install ──────────────────────────────────────────────────────────────────
# Scaffolds carry no build approvals; a build nothing approves is skipped, not fatal.
npm_config_strict_dep_builds=false "$NUB" install >/tmp/fm-$name-install.log 2>&1; inst=$?
if [ $inst -ne 0 ]; then
  log "step=install result=FAIL exit=$inst"; tail -8 /tmp/fm-$name-install.log | sed "s/^/  install> /"
  echo "VERDICT|$name|FAIL"; exit 2
fi
log "step=install result=ok"

# ── linking layout: realpath deps, do they escape the project root? ──────────
# escape → symlink into the global virtual store (GVS on); stays inside proj →
# ejected / disableGVS project-local. A single-dep probe misleads: a legitimately
# ejected dep (e.g. a `@scope/dev` CLI pulled project-local by the phantom-peer
# closure to hold a `vite` sibling symlink) sorts first and would headline
# "project-local" for an otherwise fully-GVS install. So SAMPLE the whole top
# level and report the MAJORITY layout plus the exact escape/local tallies and
# which deps are project-local — the majority is the verdict, the tally is the
# evidence. A caller-supplied <probe> still reports that one dep's own layout too.
read -r layout escaped local_ct locals rp probe_used < <(NUBPROBE="$probe" node -e '
const fs=require("fs"),p=require("path"),cwd=process.cwd();
const nm=p.join(cwd,"node_modules");
function realOf(name){try{return fs.realpathSync(p.join(nm,name));}catch{return null}}
let all=[];
for(const e of fs.readdirSync(nm)){ if(e[0]==="."||e==="node_modules") continue; if(e[0]==="@"){ for(const s of fs.readdirSync(p.join(nm,e))) all.push(e+"/"+s);} else all.push(e);}
let esc=0,loc=[];
for(const c of all){ const r=realOf(c); if(!r) continue; if(r.startsWith(cwd+p.sep)) loc.push(c); else esc++; }
const majority=(esc>=loc.length)?"gvs-store":"project-local";
// caller-supplied single probe, if any
let probe=process.env.NUBPROBE||"", prLayout="-", prName="-";
if(probe){ const r=realOf(probe); if(r){ prName=probe; prLayout=r.startsWith(cwd+p.sep)?"project-local":"gvs-store"; } }
const localsStr=loc.length?loc.slice(0,8).join(","):"none";
console.log([majority,esc,loc.length,localsStr,prLayout,prName].join(" "));
' 2>/dev/null)
log "step=linking layout=${layout:-err} escaped=${escaped:-?} project_local=${local_ct:-?} locals=${locals:-?} probe=${probe_used:-none}/${rp:-none}"

# ── dev server ────────────────────────────────────────────────────────────────
dev_result="skip"; dev_code="n/a"; dev_err="n/a"
if [ "$dev_cmd" != "-" ]; then
  devlog=/tmp/fm-$name-dev.log; : >"$devlog"
  set -m; ( cd "$proj" && exec $dev_cmd ) >"$devlog" 2>&1 & devpid=$!; set +m
  bound=""; up=""
  for i in $(seq 1 120); do
    sleep 0.5
    pp=$(grep -oE 'https?://(localhost|127\.0\.0\.1|0\.0\.0\.0|\[[0-9a-fA-F:]*\]):[0-9]+' "$devlog" 2>/dev/null | grep -oE '[0-9]+$' | head -1)
    [ -n "$pp" ] && bound="$pp"
    if [ -n "$bound" ]; then curl -s -o /dev/null "http://localhost:$bound/" 2>/dev/null && { up=1; break; }; fi
    kill -0 "$devpid" 2>/dev/null || break   # dev process died
  done
  if [ -n "$up" ]; then
    dev_code=$(curl -s -o /tmp/fm-$name-dev-body.html -w '%{http_code}' "http://localhost:$bound/" 2>/dev/null)
    # error scan: server log + served HTML for the common SSR/runtime error markers
    if grep -qiE 'error:|ReferenceError|TypeError|Cannot find|MODULE_NOT_FOUND|ERR_|Internal Server Error|failed to (load|resolve)|is not allowed|outside of .* allow list|Unhandled' "$devlog" 2>/dev/null; then dev_err="log-errors"; else dev_err="clean"; fi
    grep -qiE 'Internal Server Error|Application error|Hydration failed|500 - ' /tmp/fm-$name-dev-body.html 2>/dev/null && dev_err="html-error"
    [ "$dev_code" = "200" ] && [ "$dev_err" = "clean" ] && dev_result="ok" || dev_result="FAIL"
  else
    dev_result="FAIL"; dev_code="no-listen"
  fi
  kill_group "$devpid"; wait "$devpid" 2>/dev/null
  log "step=dev result=$dev_result http=$dev_code errors=$dev_err"
  [ "$dev_result" = "FAIL" ] && { echo "  dev-log tail:"; tail -15 "$devlog" | sed 's/^/  dev> /'; }
fi

# ── production build ──────────────────────────────────────────────────────────
build_result="skip"
if [ "$build_cmd" != "-" ]; then
  # Word-split (not eval) — consistent with how dev/preview are run; build_cmd is
  # a plain command from the trusted manifest with no shell metacharacters.
  ( cd "$proj" && $build_cmd ) >/tmp/fm-$name-build.log 2>&1; bx=$?
  [ $bx -eq 0 ] && build_result="ok" || build_result="FAIL"
  log "step=build result=$build_result exit=$bx"
  [ "$build_result" = "FAIL" ] && { echo "  build-log tail:"; tail -20 /tmp/fm-$name-build.log | sed 's/^/  build> /'; }
fi

# ── serve the built output (preview / start) ─────────────────────────────────
prev_result="skip"; prev_code="n/a"
if [ "$preview_cmd" != "-" ] && [ "$build_result" != "FAIL" ]; then
  prevlog=/tmp/fm-$name-preview.log; : >"$prevlog"
  set -m; ( cd "$proj" && exec $preview_cmd ) >"$prevlog" 2>&1 & prevpid=$!; set +m
  pbound=""; pup=""
  for i in $(seq 1 120); do
    sleep 0.5
    pp=$(grep -oE 'https?://(localhost|127\.0\.0\.1|0\.0\.0\.0|\[[0-9a-fA-F:]*\]):[0-9]+' "$prevlog" 2>/dev/null | grep -oE '[0-9]+$' | head -1)
    [ -n "$pp" ] && pbound="$pp"
    if [ -n "$pbound" ]; then curl -s -o /dev/null "http://localhost:$pbound/" 2>/dev/null && { pup=1; break; }; fi
    kill -0 "$prevpid" 2>/dev/null || break
  done
  if [ -n "$pup" ]; then
    prev_code=$(curl -s -o /dev/null -w '%{http_code}' "http://localhost:$pbound/" 2>/dev/null)
    [ "$prev_code" = "200" ] && prev_result="ok" || prev_result="FAIL"
  else
    prev_result="FAIL"; prev_code="no-listen"
  fi
  kill_group "$prevpid"; wait "$prevpid" 2>/dev/null
  log "step=preview result=$prev_result http=$prev_code"
  [ "$prev_result" = "FAIL" ] && { echo "  preview-log tail:"; tail -15 "$prevlog" | sed 's/^/  prev> /'; }
fi

# ── verdict ──────────────────────────────────────────────────────────────────
verdict="PASS"
for r in "$dev_result" "$build_result" "$prev_result"; do [ "$r" = "FAIL" ] && verdict="FAIL"; done
echo "VERDICT|$name|$verdict|dev=$dev_result build=$build_result preview=$prev_result layout=${layout:-err}"
