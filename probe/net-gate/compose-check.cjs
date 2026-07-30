const cp = require("node:child_process");
const checks = [];
const ok = (n, v, extra="") => checks.push(`${v ? "PASS" : "FAIL"}  ${n} ${extra}`);

// 1. execSync buffers correctly through both patched layers
try { ok("execSync returns stdout", cp.execSync("echo hello").toString().trim() === "hello"); }
catch (e) { ok("execSync returns stdout", false, e.message); }

// 2. execFileSync
try { ok("execFileSync returns stdout", cp.execFileSync(process.execPath, ["-e","process.stdout.write('x')"]).toString() === "x"); }
catch (e) { ok("execFileSync returns stdout", false, e.message); }

// 3. spawnSync with explicit env still sees the env we passed
const r = cp.spawnSync(process.execPath, ["-e","process.stdout.write(process.env.MYVAR||'MISSING')"], { encoding:"utf8", env:{ ...process.env, MYVAR:"kept" }});
ok("spawnSync preserves caller env", r.stdout === "kept", `got=${JSON.stringify(r.stdout)}`);

// 4. spawnSync non-zero status still reported
const r2 = cp.spawnSync(process.execPath, ["-e","process.exit(3)"], { encoding:"utf8" });
ok("spawnSync surfaces exit status", r2.status === 3, `got=${r2.status}`);

// 5. async spawn stdout still delivered
const c = cp.spawn(process.execPath, ["-e","process.stdout.write('async-ok')"], { stdio:["ignore","pipe","pipe"] });
let out=""; c.stdout.on("data",d=>out+=d);
c.on("close", () => {
  ok("async spawn delivers stdout", out === "async-ok", `got=${JSON.stringify(out)}`);
  // 6. the gate is still armed after all that patching
  const net = require("node:net");
  const s = net.connect(80, "192.168.12.189", () => { ok("gate still armed", false, "connection succeeded"); done(); });
  s.on("error", (e) => { ok("gate still armed", e.nubReason === "ERR_NUB_JAIL_NET_DENIED", e.code+"/"+e.nubReason); done(); });
});
function done(){ console.log(checks.join("\n")); process.exit(checks.some(c=>c.startsWith("FAIL"))?1:0); }
