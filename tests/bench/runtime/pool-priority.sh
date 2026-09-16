#!/bin/bash
# Does demoting the EXTRA pool threads to a low priority make a bigger pool safe on a busy host?
# Linux only: it renices threads through /proc/self/task and os.setPriority(tid). Runs at the repo
# root on a Linux box, which is the `remote-build --job adhoc` contract:
#   nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/pool-priority.sh --machine c3d-standard-16 --detach
# libuv names its workers "libuv-worker"; on Linux os.setPriority(tid, nice) targets one thread.
# Conditions on a bcrypt route under autocannon, on an idle box and beside a CPU hog on 12 of the
# 16 vCPUs: pool 4 | pool 16 | pool 16 with threads 5..16 at nice 10 | same at nice 19.
# Reports the server's req/s AND the hog's loop rate, so the co-tenant's share is visible.
set -u
ARCH=$(uname -m); case "$ARCH" in x86_64) NA=x64;; aarch64|arm64) NA=arm64;; *) echo "unknown arch $ARCH"; exit 1;; esac
NV=${NODE_VERSION:-v26.8.1}
W=$(mktemp -d /tmp/pp.XXXX); cd "$W" || exit 1; pwd; nproc; uptime
curl -fsSL "https://nodejs.org/dist/$NV/node-$NV-linux-$NA.tar.xz" -o node.tar.xz || exit 1
mkdir n && tar -xJf node.tar.xz -C n --strip-components=1 || exit 1
N="$W/n/bin"; "$N/node" --version; NP=$(nproc)
export NODE_NO_WARNINGS=1
cat > package.json <<'EOF'
{ "name": "pool-priority", "private": true, "type": "module" }
EOF
PATH="$N:$PATH" "$N/npm" install --silent --no-audit --no-fund fastify@5 bcrypt autocannon > npm.log 2>&1 || { echo "npm install failed"; tail -20 npm.log; exit 1; }

# preload: create the pool, then renice every libuv worker after the first four
cat > renice.mjs <<'EOF'
import { readdirSync, readFileSync, statSync } from "node:fs";
import { promises as fsp } from "node:fs";
import os from "node:os";
const nice = Number(process.env.EXTRA_NICE ?? "0");
const keep = Number(process.env.KEEP_NORMAL ?? "4");
await fsp.stat(".");  // first pool use: libuv creates every thread now
const tids = readdirSync("/proc/self/task").map(Number).sort((a, b) => a - b)
  .filter((t) => { try { return readFileSync(`/proc/self/task/${t}/comm`, "utf8").trim() === "libuv-worker"; } catch { return false; } });
let demoted = 0;
if (nice) for (const t of tids.slice(keep)) { os.setPriority(t, nice); demoted++; }
const nices = tids.map((t) => readFileSync(`/proc/self/task/${t}/stat`, "utf8").split(") ")[1].split(" ")[16]);
console.log(`POOL workers=${tids.length} demoted=${demoted} nice=[${nices.join(",")}]`);
EOF
cat > server.mjs <<'EOF'
import Fastify from "fastify";
import bcrypt from "bcrypt";
const app = Fastify({ logger: false });
const hash = await bcrypt.hash("correct horse battery staple", 12);
app.get("/bcrypt", async () => ({ ok: await bcrypt.compare("correct horse battery staple", hash) }));
await app.listen({ port: 0, host: "127.0.0.1" });
console.log("PORT=" + app.server.address().port + " UV_THREADPOOL_SIZE=" + (process.env.UV_THREADPOOL_SIZE ?? "unset"));
EOF
# the co-tenant: one busy loop per process, prints its iteration rate when told to stop
cat > hog.mjs <<'EOF'
let n = 0, stop = false;
process.on("SIGTERM", () => { stop = true; });
const t0 = performance.now();
while (!stop) { for (let i = 0; i < 1e6; i++) n += i & 1; if (n < 0) break; if ((n & 0xffff) === 0) await new Promise((r) => setImmediate(r)); }
console.log(JSON.stringify({ mloops: Math.round(n / 1e6), seconds: Math.round((performance.now() - t0) / 1000) }));
EOF
# hog.mjs never yields inside the inner loop, so poll for SIGTERM between chunks instead
cat > hog.mjs <<'EOF'
let n = 0, stop = false;
process.on("SIGTERM", () => { stop = true; });
const t0 = performance.now();
(function spin() { for (let i = 0; i < 2e7; i++) n += i & 1; if (stop) { console.log(JSON.stringify({ mloops: Math.round(n / 1e6), seconds: Math.round((performance.now() - t0) / 1000) })); return; } setImmediate(spin); })();
EOF

bench() { # bench <label> <pool> <extra_nice> <hogs>
  local label=$1 pool=$2 xnice=$3 hogs=$4
  local hpids=()
  for i in $(seq 1 "$hogs"); do "$N/node" hog.mjs > "hog.$i.out" 2>&1 & hpids+=($!); done
  sleep 1
  UV_THREADPOOL_SIZE=$pool EXTRA_NICE=$xnice "$N/node" --import ./renice.mjs server.mjs > server.out 2>&1 &
  local pid=$!
  for i in $(seq 1 300); do grep -q PORT= server.out && break; sleep 0.2; done
  local port; port=$(sed -n 's/PORT=\([0-9]*\).*/\1/p' server.out | head -1)
  if [ -z "$port" ]; then echo "$label: server failed"; cat server.out; kill $pid "${hpids[@]}" 2>/dev/null; return; fi
  local poolline; poolline=$(grep POOL server.out)
  local res; res=$(PATH="$N:$PATH" ./node_modules/.bin/autocannon -c 64 -d 15 --json "http://127.0.0.1:$port/bcrypt" 2>/dev/null | "$N/node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const j=JSON.parse(s);console.log(JSON.stringify({rps:Math.round(j.requests.average*10)/10,p50:j.latency.p50,p99:j.latency.p99,errors:j.errors}))})')
  kill $pid; wait $pid 2>/dev/null
  local hog=0
  if [ "$hogs" -gt 0 ]; then
    kill -TERM "${hpids[@]}" 2>/dev/null; wait "${hpids[@]}" 2>/dev/null
    hog=$(cat hog.*.out | "$N/node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{let m=0,sec=0;for(const l of s.split("\n"))if(l.trim()){const j=JSON.parse(l);m+=j.mloops;sec=j.seconds}console.log(Math.round(m/Math.max(sec,1)))})')
    rm -f hog.*.out
  fi
  echo "$label pool=$pool nice=$xnice hogs=$hogs $poolline server=$res hog_mloops_per_s=$hog"
  echo "ROW {\"bench\":\"pool-priority\",\"label\":\"$label\",\"pool\":$pool,\"nice\":$xnice,\"hogs\":$hogs,\"server\":$res,\"hogRate\":$hog}"
}
HOGS=$(( NP * 3 / 4 ))
echo "=== bcrypt cost 12, autocannon -c 64 -d 15, $NP vCPU, hogs=$HOGS busy-loop processes when present, 2 rounds ==="
for r in 1 2; do echo "--- round $r ---"
  for hogs in 0 "$HOGS"; do
    bench "pool4"        4     0  "$hogs"
    bench "pool$NP"      "$NP" 0  "$hogs"
    bench "pool${NP}-n10" "$NP" 10 "$hogs"
    bench "pool${NP}-n19" "$NP" 19 "$hogs"
  done
done
echo "DONE"
