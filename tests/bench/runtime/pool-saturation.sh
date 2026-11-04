#!/bin/bash
# Request latency on a saturated box: node (4 threads) vs nub (cores, extras at nice 10) vs plain
# node with the pool at the core count and no demotion, on a bcrypt route beside 0, 3/4, 1x and 2x
# vCPU busy-loop processes. Each cell runs twice: closed-loop (autocannon -c 64, measures capacity)
# and open-loop at a fixed offered rate (-R $RATE, default 12/s, measures what a request waits).
# Reports rps and p50/p90/p97.5/p99/max, plus the busy-loops' rate. The busy-loops share the
# server's cgroup. Runs at the repo root with NUB_BIN set (the `remote-build --job adhoc` contract):
#   nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/pool-saturation.sh --machine c3d-standard-16 --detach
set -u
echo "NUB_BIN=$NUB_BIN"; "$NUB_BIN" --version
ARCH=$(uname -m); case "$ARCH" in x86_64) NA=x64;; aarch64|arm64) NA=arm64;; *) echo "unknown arch $ARCH"; exit 1;; esac
NV=${NODE_VERSION:-v26.8.1}
W=$(mktemp -d /tmp/pool-saturation.XXXX); cd "$W" || exit 1; pwd; nproc; uptime
curl -fsSL "https://nodejs.org/dist/$NV/node-$NV-linux-$NA.tar.xz" -o node.tar.xz || exit 1
mkdir n && tar -xJf node.tar.xz -C n --strip-components=1 || exit 1
N="$W/n/bin"; "$N/node" --version; NP=$(nproc)
export NODE_NO_WARNINGS=1
cat > package.json <<'EOF2'
{ "name": "pool-saturation", "private": true, "type": "module" }
EOF2
PATH="$N:$PATH" "$N/npm" install --silent --no-audit --no-fund fastify@5 bcrypt autocannon > npm.log 2>&1 || { echo "npm install failed"; tail -20 npm.log; exit 1; }
cat > server.mjs <<'EOF2'
import Fastify from "fastify";
import bcrypt from "bcrypt";
import { readdirSync, readFileSync } from "node:fs";
const app = Fastify({ logger: false });
const hash = await bcrypt.hash("correct horse battery staple", 12);
app.get("/bcrypt", async () => ({ ok: await bcrypt.compare("correct horse battery staple", hash) }));
await app.listen({ port: 0, host: "127.0.0.1" });
const nices = [];
for (const d of readdirSync("/proc/self/task").map(Number).filter(Boolean).sort((a, b) => a - b)) {
  let comm = ""; try { comm = readFileSync(`/proc/self/task/${d}/comm`, "latin1").trim(); } catch {}
  if (comm !== "libuv-worker") continue;
  const stat = readFileSync(`/proc/self/task/${d}/stat`, "latin1");
  nices.push(Number(stat.slice(stat.lastIndexOf(")") + 2).split(" ")[16]));
}
console.log("PORT=" + app.server.address().port + " env=" + (process.env.UV_THREADPOOL_SIZE ?? "unset") + " workers=" + nices.length + " nices=" + nices.join(","));
EOF2
cat > hog.mjs <<'EOF2'
let n = 0, stop = false;
process.on("SIGTERM", () => { stop = true; });
const t0 = performance.now();
(function spin() { for (let i = 0; i < 2e7; i++) n += i & 1; if (stop) { console.log(JSON.stringify({ mloops: Math.round(n / 1e6), seconds: Math.round((performance.now() - t0) / 1000) })); return; } setImmediate(spin); })();
EOF2
bench() { # bench <label> <hogs> <mode> <cmd...>   mode: closed (-c 64) | open (-R $RATE)
  local label=$1 hogs=$2 mode=$3; shift 3
  local hpids=()
  for i in $(seq 1 "$hogs"); do "$N/node" hog.mjs > "hog.$i.out" 2>&1 & hpids+=($!); done
  sleep 1
  PATH="$N:$PATH" "$@" server.mjs > server.out 2>&1 &
  local pid=$!
  for i in $(seq 1 300); do grep -q PORT= server.out && break; sleep 0.2; done
  local port; port=$(sed -n 's/PORT=\([0-9]*\).*/\1/p' server.out | head -1)
  if [ -z "$port" ]; then echo "$label: server failed"; cat server.out; kill $pid "${hpids[@]}" 2>/dev/null; return; fi
  local pool; pool=$(grep PORT= server.out | sed 's/^PORT=[0-9]* //')
  local res; res=$(PATH="$N:$PATH" ./node_modules/.bin/autocannon -c 64 -d $DUR $( [ "$mode" = open ] && echo "-R $RATE" ) --json "http://127.0.0.1:$port/bcrypt" 2>/dev/null | "$N/node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const j=JSON.parse(s);console.log(JSON.stringify({rps:Math.round(j.requests.average*10)/10,n:j.requests.total,p50:j.latency.p50,p90:j.latency.p90,p97_5:j.latency.p97_5,p99:j.latency.p99,max:j.latency.max,errors:j.errors,timeouts:j.timeouts}))})')
  kill $pid; wait $pid 2>/dev/null
  local hog=0
  if [ "$hogs" -gt 0 ]; then
    kill -TERM "${hpids[@]}" 2>/dev/null; wait "${hpids[@]}" 2>/dev/null
    hog=$(cat hog.*.out | "$N/node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{let m=0,sec=0;for(const l of s.split("\n"))if(l.trim()){const j=JSON.parse(l);m+=j.mloops;sec=j.seconds}console.log(Math.round(m/Math.max(sec,1)))})')
    rm -f hog.*.out
  fi
  echo "$label $mode hogs=$hogs [$pool] server=$res hog_mloops_per_s=$hog"
  echo "ROW {\"bench\":\"pool-saturation\",\"label\":\"$label\",\"mode\":\"$mode\",\"hogs\":$hogs,\"pool\":\"$pool\",\"server\":$res,\"hogRate\":$hog}"
}
DUR=${DUR:-30}; RATE=${RATE:-12}
echo "=== bcrypt cost 12, $NP vCPU, $DUR s per cell; closed = autocannon -c 64, open = -R $RATE req/s; hogs are nice-0 spinners in the SAME cgroup ==="
for hogs in 0 $(( NP * 3 / 4 )) "$NP" $(( NP * 2 )); do for mode in closed open; do
  bench node   "$hogs" "$mode" "$N/node"
  bench nub    "$hogs" "$mode" "$NUB_BIN"
  bench pool16 "$hogs" "$mode" env UV_THREADPOOL_SIZE="$NP" "$N/node"
done; done
echo "DONE"
