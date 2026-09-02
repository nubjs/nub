// Re-measure win32 build-jail grants on a binary containing `46b623e352`.
//
// SCORING IS BY LOCATED PRODUCT, never exit code: these packages swallow failures and exit 0. The
// product is a SIGNATURE — the sorted (relative path, size) set of the package's own directory plus
// whichever tool-cache leaf it writes into — compared against the JAIL-OFF arm, which is what
// defines the product in the first place. A count alone is not enough: four packages in the first
// batch produced an identical file COUNT in every arm including the empty one, so the count had no
// discriminating power and only the named error did.
//
// THE LEAF NORMALISATION IS THE SUBTLE PART. Jail off, playwright writes to `%LOCALAPPDATA%\
// ms-playwright`; jailed, `redirect_playwright_browsers` points it at
// `$cache/nub/pm/tools/ms-playwright`. Those are different absolute paths holding the same product,
// so the signature takes each side's own root and compares the RELATIVE entries. Comparing absolute
// paths would score a correct redirect as a total product loss.
//
// ⛔ THE LEAVES ARE DELETED AND ASSERTED ABSENT BEFORE EVERY ARM. The defect being re-measured —
// `push_rw_path` stamps `FsOrigin::Speculative`, `derive_grants` DROPS such a rule when its path is
// absent — only bites where the leaf does not already exist. A warm box makes every arm green and
// measures nothing.
//
// LADDER WITH EARLY EXIT, cheapest rung first. The moment a rung reproduces the jail-off product the
// answer is known and the wider rungs are skipped, because a wider grant cannot un-produce it. That
// is what makes a batch this size fit in the runner budget.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

const NUB = process.env.NUB_BIN;
const ROOT = process.env.WR_ROOT;
const CACHE = process.env.LOCALAPPDATA;
const TOOLS = path.join(CACHE, "nub", "pm", "tools");
const LEAF_NAMES = ["npm-prefix", "ms-playwright", "electron-cache"];
const LEAVES = LEAF_NAMES.map((l) => path.join(TOOLS, l));
const log = (...a) => console.log(...a);

function nuke(p) {
  if (!fs.existsSync(p)) return true;
  const rm = () => { try { fs.rmSync(p, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch {} };
  rm();
  if (fs.existsSync(p)) {
    spawnSync("takeown", ["/F", p, "/R", "/D", "Y"], { stdio: "ignore" });
    spawnSync("icacls", [p, "/reset", "/T", "/C", "/Q"], { stdio: "ignore" });
    rm();
  }
  return !fs.existsSync(p);
}

/** Sorted `relpath|size` entries under `root`, prefixed with `tag`. */
function sig(root, tag, out = [], base = root, depth = 0) {
  if (depth > 14 || !fs.existsSync(root)) return out;
  let ents; try { ents = fs.readdirSync(root, { withFileTypes: true }); } catch { return out; }
  for (const e of ents) {
    const full = path.join(root, e.name);
    if (e.isDirectory()) sig(full, tag, out, base, depth + 1);
    else { try { out.push(`${tag}|${path.relative(base, full).replace(/\\/g, "/")}|${fs.statSync(full).size}`); } catch {} }
  }
  return out;
}
const jaccard = (a, b) => {
  const A = new Set(a), B = new Set(b);
  if (!A.size && !B.size) return 1;
  let inter = 0; for (const x of A) if (B.has(x)) inter++;
  return inter / (A.size + B.size - inter);
};

const shipped = JSON.parse(fs.readFileSync(process.env.WR_CATALOG, "utf8"));
function catalogWith(pkg, grant) {
  const c = JSON.parse(JSON.stringify(shipped));
  const notes = c.packages[pkg]?.default?.notes || "re-measurement arm";
  c.packages[pkg] = { default: { ...grant, notes } };
  return JSON.stringify(c, null, 1);
}

const RIGCHECK = "__rigcheck__";
const RIGCHECK_TARGET = "C:\\Windows\\Temp\\wr-rigcheck-outside.txt";

function makeFixture(dir, pkg, version, jailOff) {
  fs.mkdirSync(dir, { recursive: true });
  if (pkg === RIGCHECK) {
    const dep = path.join(dir, "dep");
    fs.mkdirSync(dep, { recursive: true });
    fs.writeFileSync(path.join(dep, "package.json"),
      JSON.stringify({ name: "wr-rigcheck-dep", version: "1.0.0", scripts: { postinstall: "node postinstall.js" } }, null, 2));
    fs.writeFileSync(path.join(dep, "postinstall.js"), [
      "const fs = require('fs');",
      `const target = ${JSON.stringify(RIGCHECK_TARGET)};`,
      "try { fs.writeFileSync(target, 'wr ' + Date.now()); console.log('RIGCHECK_WROTE ' + target); }",
      "catch (e) { console.log('RIGCHECK_REFUSED ' + e.code); process.exit(3); }",
    ].join("\n"));
    fs.writeFileSync(path.join(dir, "package.json"),
      JSON.stringify({ name: "wr-fixture", version: "1.0.0", private: true, dependencies: { "wr-rigcheck-dep": "file:./dep" } }, null, 2));
  } else {
    fs.writeFileSync(path.join(dir, "package.json"),
      JSON.stringify({ name: "wr-fixture", version: "1.0.0", private: true, dependencies: { [pkg]: version } }, null, 2));
  }
  if (jailOff) fs.writeFileSync(path.join(dir, "nub.jsonc"), '{ "install": { "buildJail": false } }');
}

function runArm({ pkg, version, arm, grant, jailOff }) {
  const dir = path.join(ROOT, pkg.replace(/[@/]/g, "_"), arm);
  nuke(dir); makeFixture(dir, pkg, version, jailOff); nuke(RIGCHECK_TARGET);
  for (const leaf of LEAVES) nuke(leaf);
  const leavesAbsent = LEAVES.every((l) => !fs.existsSync(l));
  // Jail off, the unjailed vendor defaults hold the product; jailed, the redirect leaves do.
  for (const p of [path.join(CACHE, "ms-playwright"), path.join(CACHE, "electron")]) if (jailOff) nuke(p);

  const env = { ...process.env, NUB_JAIL_DUMP_POLICY: "1" };
  if (grant) {
    const cp = path.join(dir, "catalog.json");
    fs.writeFileSync(cp, catalogWith(pkg, grant));
    env.NUB_BUILD_JAIL_CATALOG = cp;
  }
  const out = []; let rc = 0;
  for (const argv of [["install"], ["approve-builds", "--all"]]) {
    const r = spawnSync(NUB, argv, { cwd: dir, env, encoding: "utf8", timeout: 20 * 60_000 });
    out.push(`$ nub ${argv.join(" ")} -> ${r.status}\n${r.stdout || ""}\n${r.stderr || ""}`);
    if (argv[0] === "install") rc = r.status ?? -1; else if (rc === 0) rc = r.status ?? -1;
  }
  const text = out.join("\n");
  const jd = text.split(/\r?\n/).filter((l) => l.includes("JAILDUMP"));
  const rules = jd.filter((l) => /JAILDUMP\s+\w*(Allow|Deny)/.test(l));

  const pkgDir = pkg === RIGCHECK ? path.join(dir, "node_modules", "wr-rigcheck-dep")
                                  : path.join(dir, "node_modules", ...pkg.split("/"));
  const signature = [
    ...sig(pkgDir, "pkg"),
    ...sig(jailOff ? path.join(CACHE, "ms-playwright") : path.join(TOOLS, "ms-playwright"), "pw"),
    ...sig(jailOff ? path.join(CACHE, "electron") : path.join(TOOLS, "electron-cache"), "el"),
  ].sort();
  let resolvedVersion = null;
  try { resolvedVersion = JSON.parse(fs.readFileSync(path.join(pkgDir, "package.json"), "utf8")).version; } catch {}

  return {
    pkg, version, arm, rc, resolvedVersion, leavesAbsent,
    jaildumpLines: jd.length, jaildumpRuleLines: rules.length,
    leafRulesInPolicy: LEAF_NAMES.filter((b) =>
      rules.some((r) => r.toLowerCase().includes(`tools\\${b}`) || r.toLowerCase().includes(`tools/${b}`))),
    catalogInForce: grant ? /catalog OVERRIDDEN from/.test(text) : "shipped(no-override)",
    catalogRejected: /catalog override at .* was REJECTED/.test(text),
    rigcheckWrote: /RIGCHECK_WROTE/.test(text), rigcheckRefused: /RIGCHECK_REFUSED/.test(text),
    rigcheckTargetExists: fs.existsSync(RIGCHECK_TARGET),
    signature, sigCount: signature.length,
    errorTells: [...new Set((text.match(/EPERM[^\n]{0,100}|ENOTFOUND[^\n]{0,60}|EACCES[^\n]{0,100}|ERR_NUB_[A-Z_]+|operation not permitted[^\n]{0,70}/g) || []))].slice(0, 6),
    tail: text.slice(-2500),
  };
}

// Cheapest rung first. `shipped` is NOT in the ladder: it is only reached as a diagnostic when
// nothing narrowed, since a rung passing implies every wider rung would too.
const LADDER = [
  { arm: "red",    grant: {},                                                  verdict: "base (nothing beyond the baseline)" },
  { arm: "narrow", grant: { network: true },                                   verdict: "{network}" },
  { arm: "deps",   grant: { write: { deps: true }, network: true },            verdict: "{write:{deps}} + network" },
  { arm: "mid",    grant: { write: { deps: true, project: true }, network: true }, verdict: "{write:{deps,project}} + network" },
];

const specs = JSON.parse(process.env.WR_SPECS);
const results = [];
const verdicts = [];
const save = () => {
  fs.writeFileSync(path.join(ROOT, "results.json"), JSON.stringify(results, null, 1));
  fs.writeFileSync(path.join(ROOT, "verdicts.json"), JSON.stringify(verdicts, null, 1));
};

for (const { pkg, version } of [{ pkg: RIGCHECK, version: "local" }, ...specs]) {
  log(`\n############ ${pkg}@${version} ############`);
  const push = (r) => { results.push(r); log(JSON.stringify({ ...r, tail: undefined, signature: undefined }, null, 1)); save(); };

  const off = runArm({ pkg, version, arm: "jailoff", grant: null, jailOff: true });
  push(off);
  if (pkg === RIGCHECK) { push(runArm({ pkg, version, arm: "shipped", grant: null, jailOff: false })); continue; }

  // ⛔ A package whose jail-off arm produces NOTHING cannot be scored by product at all. Say so
  // rather than letting an empty-equals-empty comparison read as a pass at every rung.
  if (off.sigCount === 0) {
    verdicts.push({ pkg, version, verdict: "VOID", why: "jail-off produced no product to compare against" });
    log("VOID: jail-off produced no located product"); save(); continue;
  }

  let settled = null;
  for (const rung of LADDER) {
    const r = runArm({ pkg, version, arm: rung.arm, grant: rung.grant, jailOff: false });
    r.jaccardVsJailoff = jaccard(off.signature, r.signature);
    r.exactMatch = r.jaccardVsJailoff === 1;
    push(r);
    // ⛔ Zero JAILDUMP lines means the jail never ran, which is VOID rather than a pass.
    if (r.jaildumpLines === 0) { settled = { verdict: "VOID", why: "no JAILDUMP: the jail never ran (package has no lifecycle script)" }; break; }
    if (r.jaccardVsJailoff >= 0.98) { settled = { verdict: rung.verdict, arm: rung.arm, jaccard: r.jaccardVsJailoff, rc: r.rc }; break; }
  }
  if (!settled) {
    const sh = runArm({ pkg, version, arm: "shipped", grant: null, jailOff: false });
    sh.jaccardVsJailoff = jaccard(off.signature, sh.signature);
    push(sh);
    settled = { verdict: "STAYS WIDE", why: `no rung reproduced the jail-off product; shipped grant itself scores ${sh.jaccardVsJailoff.toFixed(3)}`, shippedRc: sh.rc };
  }
  verdicts.push({ pkg, version, resolvedVersion: off.resolvedVersion, jailoffFiles: off.sigCount, ...settled });
  log(`VERDICT ${pkg}: ${JSON.stringify(settled)}`); save();
}

log("\n\n########## RIG CONTROL ##########");
const rOff = results.find((r) => r.pkg === RIGCHECK && r.arm === "jailoff");
const rOn = results.find((r) => r.pkg === RIGCHECK && r.arm === "shipped");
log(`jail OFF wrote outside : ${rOff?.rigcheckWrote} (exists ${rOff?.rigcheckTargetExists}) <- MUST be true`);
log(`jail ON  refused       : ${rOn?.rigcheckRefused} (exists ${rOn?.rigcheckTargetExists}) <- MUST be refused`);
log(`RIG CONTROL: ${rOff?.rigcheckWrote === true && rOn?.rigcheckWrote !== true ? "PASS" : "⛔ FAIL — every verdict in this run is VOID"}`);
log(`leaf rules present on every jailed arm: ${results.filter((r) => r.arm !== "jailoff" && r.jaildumpLines > 0).every((r) => r.leafRulesInPolicy.length === 3)}`);
log("\n########## VERDICTS ##########");
for (const v of verdicts) log(`${(v.pkg + "@" + (v.resolvedVersion || v.version)).padEnd(46)} ${String(v.verdict).padEnd(42)} ${v.why || `arm=${v.arm} jaccard=${(v.jaccard ?? 0).toFixed(3)} rc=${v.rc}`}`);
