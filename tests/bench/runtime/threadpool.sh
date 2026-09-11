#!/bin/bash
# libuv threadpool size: Node's fixed 4 vs the core count Nub installs (UV_THREADPOOL_SIZE=max(4, cores)). Runs on the
# latest Node by default; the augmentation is version-independent, so the figure is drawn from that run.
# Fastify 5 routes that queue on the pool (pbkdf2, async gzip, a file read, a stat) under autocannon,
# then a dns.lookup burst. Plain `node` with the variable set on the command line, so the measurement is
# of the pool size alone and not of any other augmentation.
#
# Runs at the repo root on a Linux box, which is the `remote-build --job adhoc` contract:
#   nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/threadpool.sh --detach
# Every measurement is also printed as one machine-readable `ROW {...}` line, so a chart generator
# reads the run's output rather than numbers typed by hand (see .claude/skills/nub-charts).
set -u
ARCH=$(uname -m); case "$ARCH" in x86_64) NA=x64;; aarch64|arm64) NA=arm64;; *) echo "unknown arch $ARCH"; exit 1;; esac
NV=${NODE_VERSION:-v26.8.1}
ROUNDS=${ROUNDS:-5}
W=$(mktemp -d /tmp/tp.XXXX); cd "$W" || exit 1; pwd; nproc; uptime
curl -fsSL "https://nodejs.org/dist/$NV/node-$NV-linux-$NA.tar.xz" -o node.tar.xz || exit 1
mkdir n22 && tar -xJf node.tar.xz -C n22 --strip-components=1 || exit 1  # n22 is the dir name, whatever $NV is
N22="$W/n22/bin"; NP=$(nproc)
export NODE_NO_WARNINGS=1
cat > package.json <<'EOF'
{ "name": "tp-bench", "private": true, "type": "module" }
EOF
PATH="$N22:$PATH" "$N22/npm" install --silent --no-audit --no-fund fastify@5 @fastify/compress autocannon > npm.log 2>&1 || { echo "npm install failed"; tail -20 npm.log; exit 1; }
head -c 204800 /dev/urandom | base64 > asset.txt   # ~270 KB text file, compressible
"$N22/node" -e 'const o={rows:Array.from({length:600},(_,i)=>({id:i,name:"user"+i,email:"user"+i+"@example.com",tags:["a","b","c"],score:i*1.5}))};require("fs").writeFileSync("payload.json",JSON.stringify(o))'

cat > server.mjs <<'EOF'
import Fastify from "fastify"; import compress from "@fastify/compress"; import { readFile, stat } from "node:fs/promises"; import { pbkdf2 } from "node:crypto"; import { readFileSync } from "node:fs";
const payload = JSON.parse(readFileSync("payload.json", "utf8"));
const app = Fastify({ logger: false }); await app.register(compress, { global: false, threshold: 0 });
app.get("/gzip", { compress: { threshold: 0 } }, async (req, reply) => { reply.header("content-type", "application/json"); return payload; });
app.get("/file", async () => readFile("asset.txt", "utf8"));
app.get("/stat", async () => { const s = await stat("asset.txt"); return { size: s.size }; });
app.get("/hash", async () => new Promise((res, rej) => pbkdf2("password", "salt", 2000, 32, "sha256", (e, k) => e ? rej(e) : res({ k: k.toString("hex") }))));
await app.listen({ port: 0, host: "127.0.0.1" }); console.log("PORT=" + app.server.address().port);
EOF
bench() { # bench <pool> <route>
  local pool=$1 route=$2
  UV_THREADPOOL_SIZE=$pool "$N22/node" server.mjs > server.out 2>&1 &
  local pid=$!
  for i in $(seq 1 100); do grep -q PORT= server.out && break; sleep 0.2; done
  local port; port=$(sed -n 's/PORT=//p' server.out | head -1)
  if [ -z "$port" ]; then echo "pool=$pool $route: server failed"; cat server.out; kill $pid 2>/dev/null; return; fi
  local res; res=$(PATH="$N22:$PATH" ./node_modules/.bin/autocannon -c 50 -d 8 -H "accept-encoding: gzip" --json "http://127.0.0.1:$port$route" 2>/dev/null | "$N22/node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const j=JSON.parse(s);console.log(JSON.stringify({rps:Math.round(j.requests.average),p50:j.latency.p50,p99:j.latency.p99,errors:j.errors,non2xx:j.non2xx}))})')
  echo "pool=$pool $route: $res"
  echo "ROW {\"bench\":\"routes\",\"pool\":$pool,\"route\":\"$route\",\"result\":$res}"
  kill $pid; wait $pid 2>/dev/null
}
echo "=== routes: pool 4 (Node default) vs $NP (cores), Node $NV, autocannon -c 50 -d 8, $ROUNDS interleaved rounds ==="
for r in $(seq 1 "$ROUNDS"); do echo "--- round $r ---"; for route in /hash /gzip /file /stat; do bench 4 $route; bench "$NP" $route; done; done

cat > dns.mjs <<'EOF'
// dns.lookup burst (getaddrinfo on the threadpool): 400 lookups, concurrency 100
import { lookup } from "node:dns/promises";
const names = ["localhost", "127.0.0.1", "example.com", "nodejs.org", "github.com", "npmjs.com", "google.com", "cloudflare.com"];
const t0 = performance.now(); let done = 0;
for (let b = 0; b < 4; b++) { await Promise.all(Array.from({ length: 100 }, (_, i) => lookup(names[i % names.length]).catch(() => 0).then(() => done++))); }
console.log(JSON.stringify({ lookups: done, ms: Math.round(performance.now() - t0) }));
EOF
echo "=== dns.lookup burst, 400 lookups at concurrency 100 ==="
for r in $(seq 1 "$ROUNDS"); do for pool in 4 "$NP"; do
  out=$(UV_THREADPOOL_SIZE=$pool "$N22/node" dns.mjs)
  echo "pool=$pool: $out"
  echo "ROW {\"bench\":\"dns\",\"pool\":$pool,\"result\":$out}"
done; done
echo "DONE"
