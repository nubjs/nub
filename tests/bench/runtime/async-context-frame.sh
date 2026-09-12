#!/bin/bash
# AsyncLocalStorage on AsyncContextFrame: the implementation Node 24 made the default, which Nub
# switches on for Node 22.9–23.x with --experimental-async-context-frame.
#   node (default)  |  node with the other implementation  |  nub (augmented)  |  nub --node
# on (a) a request-shaped AsyncLocalStorage loop and (b) Fastify 5 + OpenTelemetry SDK under autocannon.
# The "other implementation" control is the frame on Node 22/23 and the legacy path
# (--no-async-context-frame) on 24+, where nub injects nothing and node equals nub by construction;
# a run there is the control that shows the figure is a Node 22 LTS claim, not a Node 26 one.
#
# Runs at the repo root with NUB_BIN set, which is the `remote-build --job adhoc` contract:
#   nub scripts/remote-build.ts --job adhoc --script tests/bench/runtime/async-context-frame.sh --detach
# Every measurement is also printed as one machine-readable `ROW {...}` line, so a chart generator
# reads the run's output rather than numbers typed by hand (see .claude/skills/nub-charts).
set -u
echo "NUB_BIN=$NUB_BIN"; "$NUB_BIN" --version
ARCH=$(uname -m); case "$ARCH" in x86_64) NA=x64;; aarch64|arm64) NA=arm64;; *) echo "unknown arch $ARCH"; exit 1;; esac
NV=${NODE_VERSION:-v26.8.1}
W=$(mktemp -d /tmp/acf.XXXX); cd "$W" || exit 1; pwd
curl -fsSL "https://nodejs.org/dist/$NV/node-$NV-linux-$NA.tar.xz" -o node.tar.xz || exit 1
mkdir n22 && tar -xJf node.tar.xz -C n22 --strip-components=1 || exit 1
N22="$W/n22/bin"; "$N22/node" --version
nproc; uptime
MAJOR=${NV#v}; MAJOR=${MAJOR%%.*}
if [ "$MAJOR" -ge 24 ]; then OTHER=--no-async-context-frame; OTHER_LABEL=node-legacy; else OTHER=--experimental-async-context-frame; OTHER_LABEL=node+flag; fi

cat > als.mjs <<'EOF'
import { AsyncLocalStorage } from "node:async_hooks";
const als = new AsyncLocalStorage();
const N = 40000; let sink = 0;
async function hop(i) { await null; sink += als.getStore().id & 1; }
async function request(id) {
  return als.run({ id }, async () => {
    for (let k = 0; k < 8; k++) await hop(k);
    await new Promise(r => setImmediate(r));
    sink += als.getStore().id & 1;
    return Promise.resolve(1).then(() => als.getStore().id);
  });
}
const t0 = performance.now();
for (let b = 0; b < N / 100; b++) { const ps = []; for (let i = 0; i < 100; i++) ps.push(request(b * 100 + i)); await Promise.all(ps); }
console.log(JSON.stringify({ version: process.version, execArgv: process.execArgv, ms: Math.round(performance.now() - t0) }));
EOF
export NODE_NO_WARNINGS=1
echo "=== (a) AsyncLocalStorage loop, 40k requests x 8 awaits, 3 rounds each ==="
loop() { # loop <label> <cmd...>
  local label=$1; shift
  local out; out=$(PATH="$N22:$PATH" "$@" als.mjs 2>&1 | tail -1)
  echo "$label $out"
  echo "ROW {\"bench\":\"als-loop\",\"label\":\"$label\",\"result\":$out}"
}
for r in 1 2 3; do
  loop "node" "$N22/node"
  loop "$OTHER_LABEL" "$N22/node" "$OTHER"
  loop "nub" "$NUB_BIN"
  loop "nub--node" "$NUB_BIN" --node
done

echo "=== (b) Fastify 5 + OpenTelemetry SDK, autocannon -c 50 -d 10 ==="
cat > package.json <<'EOF'
{ "name": "acf-bench", "private": true, "type": "module" }
EOF
PATH="$N22:$PATH" "$N22/npm" install --silent --no-audit --no-fund fastify@5 @opentelemetry/sdk-node @opentelemetry/api @opentelemetry/instrumentation-http @opentelemetry/instrumentation-fastify autocannon > npm.log 2>&1 || { echo "npm install failed"; tail -20 npm.log; exit 1; }
cat > server.mjs <<'EOF'
import { NodeSDK } from "@opentelemetry/sdk-node";
import { HttpInstrumentation } from "@opentelemetry/instrumentation-http";
import { FastifyInstrumentation } from "@opentelemetry/instrumentation-fastify";
import { trace, context } from "@opentelemetry/api";
const sdk = new NodeSDK({ instrumentations: [new HttpInstrumentation(), new FastifyInstrumentation()] });
sdk.start();
const Fastify = (await import("fastify")).default;
const app = Fastify({ logger: false });
const tracer = trace.getTracer("bench");
async function db(i) { return tracer.startActiveSpan("db" + i, async (s) => { await new Promise(r => setImmediate(r)); s.end(); return { i, ctx: !!trace.getSpan(context.active()) }; }); }
app.get("/", async () => { const rows = []; for (let i = 0; i < 5; i++) rows.push(await db(i)); return { ok: true, rows, execArgv: process.execArgv }; });
await app.listen({ port: 0, host: "127.0.0.1" });
console.log("PORT=" + app.server.address().port);
EOF
bench() { # bench <label> <cmd...>
  local label=$1; shift
  "$@" server.mjs > server.out 2>&1 &
  local pid=$!
  for i in $(seq 1 100); do grep -q PORT= server.out && break; sleep 0.2; done
  local port; port=$(sed -n 's/PORT=//p' server.out | head -1)
  if [ -z "$port" ]; then echo "$label: server failed"; cat server.out; kill $pid 2>/dev/null; return; fi
  local args; args=$(curl -s "http://127.0.0.1:$port/" | sed -n 's/.*"execArgv":\(\[[^]]*\]\).*/\1/p')
  local res; res=$(PATH="$N22:$PATH" ./node_modules/.bin/autocannon -c 50 -d 10 --json "http://127.0.0.1:$port/" 2>/dev/null | "$N22/node" -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>{const j=JSON.parse(s);console.log(JSON.stringify({rps:Math.round(j.requests.average),p50:j.latency.p50,p99:j.latency.p99,errors:j.errors}))})')
  echo "$label execArgv=$args $res"
  echo "ROW {\"bench\":\"fastify-otel\",\"label\":\"$label\",\"execArgv\":$args,\"result\":$res}"
  kill $pid; wait $pid 2>/dev/null
}
for r in 1 2 3; do
  echo "--- round $r ---"
  PATH="$N22:$PATH" bench "node" "$N22/node"
  PATH="$N22:$PATH" bench "$OTHER_LABEL" "$N22/node" "$OTHER"
  PATH="$N22:$PATH" bench "nub" "$NUB_BIN"
  PATH="$N22:$PATH" bench "nub--node" "$NUB_BIN" --node
done
echo "DONE"
