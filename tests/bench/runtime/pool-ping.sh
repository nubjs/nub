#!/bin/bash
# Event-loop latency while the pool is saturated: a /ping route at 50 req/s (autocannon -R 50 -c 10)
# measured while a bcrypt route runs closed-loop at -c 64, for node (4 threads), nub (cores, extras at
# nice 10) and plain node with the pool at the core count and no demotion, beside 0, 1x and 2x vCPU
# busy-loop processes in the server's cgroup. Two rounds. Runs at the repo root with NUB_BIN set:
#   nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/pool-ping.sh --machine c3d-standard-16 --detach
set -u
echo "NUB_BIN=$NUB_BIN"; "$NUB_BIN" --version
ARCH=$(uname -m); case "$ARCH" in x86_64) NA=x64;; aarch64|arm64) NA=arm64;; *) echo "unknown arch $ARCH"; exit 1;; esac
NV=${NODE_VERSION:-v26.8.1}
W=$(mktemp -d /tmp/pool-ping.XXXX); cd "$W" || exit 1; pwd; nproc; uptime
curl -fsSL "https://nodejs.org/dist/$NV/node-$NV-linux-$NA.tar.xz" -o node.tar.xz || exit 1
mkdir n && tar -xJf node.tar.xz -C n --strip-components=1 || exit 1
N="$W/n/bin"; "$N/node" --version; NP=$(nproc)
export NODE_NO_WARNINGS=1
cat > package.json <<'EOF2'
{ "name": "pool-ping", "private": true, "type": "module" }
EOF2
PATH="$N:$PATH" "$N/npm" install --silent --no-audit --no-fund fastify@5 bcrypt autocannon > npm.log 2>&1 || { echo "npm install failed"; tail -20 npm.log; exit 1; }
cat > server.mjs <<'EOF2'
import Fastify from "fastify";
import bcrypt from "bcrypt";
import { readdirSync, readFileSync } from "node:fs";
const app = Fastify({ logger: false });
const hash = await bcrypt.hash("correct horse battery staple", 12);
app.get("/ping", async () => ({ ok: 1 }));
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
  local AC=./node_modules/.bin/autocannon SUM='let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const j=JSON.parse(s);console.log(JSON.stringify({rps:Math.round(j.requests.average*10)/10,n:j.requests.total,p50:j.latency.p50,p90:j.latency.p90,p99:j.latency.p99,max:j.latency.max,errors:j.errors}))})'
  PATH="$N:$PATH" $AC -c 64 -d $DUR --json "http://127.0.0.1:$port/bcrypt" 2>/dev/null > bc.json &
  local bcpid=$!
  sleep 3
  local res; res=$(PATH="$N:$PATH" $AC -c 10 -R 50 -d $(( DUR - 6 )) --json "http://127.0.0.1:$port/ping" 2>/dev/null | "$N/node" -e "$SUM")
  wait $bcpid; local bc; bc=$("$N/node" -e "$SUM" < bc.json)
  res="{\"ping\":$res,\"bcrypt\":$bc}"
  kill $pid; wait $pid 2>/dev/null
  local hog=0
  if [ "$hogs" -gt 0 ]; then
    kill -TERM "${hpids[@]}" 2>/dev/null; wait "${hpids[@]}" 2>/dev/null
    hog=$(cat hog.*.out | "$N/node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{let m=0,sec=0;for(const l of s.split("\n"))if(l.trim()){const j=JSON.parse(l);m+=j.mloops;sec=j.seconds}console.log(Math.round(m/Math.max(sec,1)))})')
    rm -f hog.*.out
  fi
  echo "$label $mode hogs=$hogs [$pool] server=$res hog_mloops_per_s=$hog"
  echo "ROW {\"bench\":\"pool-ping\",\"label\":\"$label\",\"mode\":\"$mode\",\"hogs\":$hogs,\"pool\":\"$pool\",\"server\":$res,\"hogRate\":$hog}"
}
DUR=${DUR:-30}
echo "=== /ping at 50 req/s (10 conns) while /bcrypt runs closed-loop -c 64; $NP vCPU; hogs are nice-0 spinners in the SAME cgroup ==="
for r in 1 2; do for hogs in 0 "$NP" $(( NP * 2 )); do
  bench node   "$hogs" ping "$N/node"
  bench nub    "$hogs" ping "$NUB_BIN"
  bench pool16 "$hogs" ping env UV_THREADPOOL_SIZE="$NP" "$N/node"
done; done
echo "DONE"
