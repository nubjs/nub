// Re-measure win32 build-jail grants on a binary containing `46b623e352`.
//
// TWO INDEPENDENT HYPOTHESES, one rig.
//
// H1 — the redirect-leaf defect. `$cache/nub/pm/tools/{npm-prefix,ms-playwright,electron-cache}`
// carry an unconditional read-write grant while their `tools` parent stays read-only on purpose.
// `push_rw_path` stamps `FsOrigin::Speculative`, and `derive_grants` (backend/windows.rs) DROPS
// such a rule when its path is absent — so before `46b623e352` the grant vanished on any machine
// that had not already run an unjailed install, the package's own mkdir hit the read-only parent,
// and the ladder escalated until `write.userHome` worked. Predicts the playwright/electron
// witnesses narrow to `{network}`.
//
// H2 — the two Windows defects fixed in early August (a missing `container_profile`, called in the
// source "THE SINGLE LARGEST CAUSE OF WHOLE-DISK GRANTS ON WINDOWS", and a second that "drove ~17
// packages to a whole-disk grant that they do not need"). Predicts the `write:"disk"` population
// narrows. `write:"disk"` is the only rung that declines the LowBox token altogether, so narrowing
// one restores OS-level egress confinement — a strictly larger win than narrowing a `userHome`
// cell, which already had the token.
//
// ⛔ THE H1 FAULT BITES ONLY WHERE THE LEAF DOES NOT ALREADY EXIST, so every arm DELETES the three
// leaves and ASSERTS their absence before launching. A warm box makes all arms green and measures
// nothing.
//
// ⛔ SCORING IS BY LOCATED PRODUCT, never exit code. Packages here swallow failures and exit 0.

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

/** Recursive delete that survives the ACL'd trees the jail leaves behind. */
function nuke(p) {
  if (!fs.existsSync(p)) return true;
  const rm = () => {
    try {
      fs.rmSync(p, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
    } catch {}
  };
  rm();
  if (fs.existsSync(p)) {
    spawnSync("takeown", ["/F", p, "/R", "/D", "Y"], { stdio: "ignore" });
    spawnSync("icacls", [p, "/reset", "/T", "/C", "/Q"], { stdio: "ignore" });
    rm();
  }
  return !fs.existsSync(p);
}

function walk(p, out = [], depth = 0) {
  if (depth > 14 || !fs.existsSync(p)) return out;
  let ents;
  try {
    ents = fs.readdirSync(p, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of ents) {
    const full = path.join(p, e.name);
    if (e.isDirectory()) walk(full, out, depth + 1);
    else {
      try {
        out.push({ path: full, size: fs.statSync(full).size });
      } catch {}
    }
  }
  return out;
}
const biggest = (f) => (f.length ? f.reduce((a, b) => (a.size > b.size ? a : b)) : null);
const shipped = JSON.parse(fs.readFileSync(process.env.WR_CATALOG, "utf8"));

/** A catalog whose target package carries exactly `grant`, replacing every band. */
function catalogWith(pkg, grant) {
  const c = JSON.parse(JSON.stringify(shipped));
  const notes = c.packages[pkg]?.default?.notes || "re-measurement arm";
  c.packages[pkg] = { default: { ...grant, notes } };
  return JSON.stringify(c, null, 1);
}

// ── the arm ladder ──────────────────────────────────────────────────────────────────────────────
// `shipped` deliberately passes NO override, so it exercises the catalog compiled into the binary
// — the true status quo — rather than a re-serialized copy of it.
const ARMS = [
  { arm: "jailoff", jailOff: true, grant: null },
  { arm: "shipped", grant: null },
  { arm: "narrow", grant: { network: true } },
  { arm: "mid", grant: { write: { deps: true, project: true }, network: true } },
  { arm: "red", grant: {} },
];

/**
 * The RIG CONTROL, and it is synthetic on purpose. A per-package `red` arm can legitimately pass
 * — a package that needs nothing is the strongest narrowing result there is — so a red arm that
 * depends on package behaviour cannot prove the rig reports failure. This fixture's postinstall
 * writes OUTSIDE every grant, so it must succeed jail-off and fail under any jail at all.
 */
const RIGCHECK = "__rigcheck__";
const RIGCHECK_TARGET = "C:\\Windows\\Temp\\wr-rigcheck-outside.txt";

function makeFixture(dir, pkg, version, jailOff) {
  fs.mkdirSync(dir, { recursive: true });
  if (pkg === RIGCHECK) {
    // A local dependency so the confined script is OUR script. `file:` keeps the registry out of
    // the control entirely.
    const dep = path.join(dir, "dep");
    fs.mkdirSync(dep, { recursive: true });
    fs.writeFileSync(
      path.join(dep, "package.json"),
      JSON.stringify({ name: "wr-rigcheck-dep", version: "1.0.0", scripts: { postinstall: "node postinstall.js" } }, null, 2),
    );
    fs.writeFileSync(
      path.join(dep, "postinstall.js"),
      [
        "const fs = require('fs');",
        `const target = ${JSON.stringify(RIGCHECK_TARGET)};`,
        "try { fs.writeFileSync(target, 'wr ' + Date.now()); console.log('RIGCHECK_WROTE ' + target); }",
        "catch (e) { console.log('RIGCHECK_REFUSED ' + e.code + ' ' + e.message); process.exit(3); }",
      ].join("\n"),
    );
    fs.writeFileSync(
      path.join(dir, "package.json"),
      JSON.stringify({ name: "wr-fixture", version: "1.0.0", private: true, dependencies: { "wr-rigcheck-dep": "file:./dep" } }, null, 2),
    );
  } else {
    fs.writeFileSync(
      path.join(dir, "package.json"),
      JSON.stringify({ name: "wr-fixture", version: "1.0.0", private: true, dependencies: { [pkg]: version } }, null, 2),
    );
  }
  if (jailOff) fs.writeFileSync(path.join(dir, "nub.jsonc"), '{ "install": { "buildJail": false } }');
}

function runArm({ pkg, version, arm, grant, jailOff }) {
  const dir = path.join(ROOT, pkg.replace(/[@/]/g, "_"), arm);
  nuke(dir);
  makeFixture(dir, pkg, version, jailOff);
  nuke(RIGCHECK_TARGET);

  // ── the H1 precondition, re-established per arm ──────────────────────────────────────────────
  for (const leaf of LEAVES) nuke(leaf);
  const leafStateBefore = Object.fromEntries(LEAF_NAMES.map((n, i) => [n, fs.existsSync(LEAVES[i])]));
  const leavesAbsentBeforeLaunch = Object.values(leafStateBefore).every((v) => v === false);

  const env = { ...process.env, NUB_JAIL_DUMP_POLICY: "1" };
  if (grant) {
    const catPath = path.join(dir, "catalog.json");
    fs.writeFileSync(catPath, catalogWith(pkg, grant));
    env.NUB_BUILD_JAIL_CATALOG = catPath;
  }

  const out = [];
  let rc = 0;
  for (const argv of [["install"], ["approve-builds", "--all"]]) {
    const r = spawnSync(NUB, argv, { cwd: dir, env, encoding: "utf8", timeout: 25 * 60_000 });
    out.push(`$ nub ${argv.join(" ")}  -> ${r.status}\n${r.stdout || ""}\n${r.stderr || ""}`);
    if (argv[0] === "install") rc = r.status ?? -1;
    else if (rc === 0) rc = r.status ?? -1;
  }
  const text = out.join("\n");

  const jaildump = text.split(/\r?\n/).filter((l) => l.includes("JAILDUMP"));
  const ruleLines = jaildump.filter((l) => /JAILDUMP\s+\w*(Allow|Deny)/.test(l));
  const hasLeaf = (b) => ruleLines.some((r) => r.toLowerCase().includes(`tools\\${b}`) || r.toLowerCase().includes(`tools/${b}`));

  const pkgDir = pkg === RIGCHECK ? path.join(dir, "node_modules", "wr-rigcheck-dep") : path.join(dir, "node_modules", ...pkg.split("/"));
  const pkgFiles = walk(pkgDir);
  let resolvedVersion = null;
  try {
    resolvedVersion = JSON.parse(fs.readFileSync(path.join(pkgDir, "package.json"), "utf8")).version;
  } catch {}

  return {
    pkg,
    version,
    arm,
    rc,
    resolvedVersion,
    leavesAbsentBeforeLaunch,
    leafStateBefore,
    // ⛔ Zero JAILDUMP lines on a jailed arm = the jail never ran = VOID, not pass.
    jaildumpLines: jaildump.length,
    jaildumpRuleLines: ruleLines.length,
    // ⛔ THE PREMISE EVERYTHING RESTS ON: are the three leaves PRESENT in the compiled rule list?
    leafRulesInPolicy: LEAF_NAMES.filter(hasLeaf),
    toolsRuleLines: ruleLines.filter((r) => /tools/i.test(r)).slice(0, 8),
    // A rejected override means the COMPILED catalog answered and the arm measured the wrong thing.
    catalogInForce: grant ? /catalog OVERRIDDEN from/.test(text) : "shipped(no-override)",
    catalogRejected: /catalog override at .* was REJECTED/.test(text),
    rigcheckWrote: /RIGCHECK_WROTE/.test(text),
    rigcheckRefused: /RIGCHECK_REFUSED/.test(text),
    rigcheckTargetExists: fs.existsSync(RIGCHECK_TARGET),
    products: {
      pkgFiles: pkgFiles.length,
      pkgBiggest: biggest(pkgFiles),
      msPlaywrightLeaf: walk(path.join(TOOLS, "ms-playwright")).length,
      msPlaywrightBiggest: biggest(walk(path.join(TOOLS, "ms-playwright"))),
      electronCacheLeaf: walk(path.join(TOOLS, "electron-cache")).length,
      npmPrefixLeaf: walk(path.join(TOOLS, "npm-prefix")).length,
      unjailedMsPlaywright: walk(path.join(CACHE, "ms-playwright")).length,
      unjailedElectron: walk(path.join(CACHE, "electron")).length,
    },
    errorTells: [...new Set((text.match(/EPERM[^\n]{0,110}|ENOTFOUND[^\n]{0,70}|EACCES[^\n]{0,110}|operation not permitted[^\n]{0,80}|ERR_NUB_[A-Z_]+/g) || []))].slice(0, 8),
    tail: text.slice(-3500),
  };
}

// ── main ────────────────────────────────────────────────────────────────────────────────────────
const specs = JSON.parse(process.env.WR_SPECS);
const results = [];
for (const { pkg, version } of [{ pkg: RIGCHECK, version: "local" }, ...specs]) {
  for (const a of ARMS) {
    // The synthetic control only needs the two ends: jail off (must write) and fully jailed
    // (must be refused). The middle rungs say nothing about it.
    if (pkg === RIGCHECK && !["jailoff", "shipped"].includes(a.arm)) continue;
    log(`\n=============== ${pkg}@${version} :: ${a.arm} ===============`);
    let r;
    try {
      r = runArm({ pkg, version, arm: a.arm, grant: a.grant, jailOff: a.jailOff });
    } catch (e) {
      r = { pkg, version, arm: a.arm, error: String(e && e.stack) };
    }
    results.push(r);
    log(JSON.stringify({ ...r, tail: undefined }, null, 1));
    log("---- tail ----\n" + (r.tail || ""));
    fs.writeFileSync(path.join(ROOT, "results.json"), JSON.stringify(results, null, 1));
  }
}

log("\n\n########## SUMMARY ##########");
log(["pkg@ver".padEnd(40), "arm".padEnd(9), "rc".padEnd(6), "jd".padEnd(6), "absent".padEnd(8), "leafRules".padEnd(38), "pkgF".padEnd(8), "msPw".padEnd(7), "elec".padEnd(6), "catOK"].join(" "));
for (const r of results)
  log(
    [
      `${r.pkg}@${r.resolvedVersion || r.version}`.slice(0, 39).padEnd(40),
      String(r.arm).padEnd(9),
      String(r.rc).padEnd(6),
      String(r.jaildumpLines).padEnd(6),
      String(r.leavesAbsentBeforeLaunch).padEnd(8),
      JSON.stringify(r.leafRulesInPolicy || []).padEnd(38),
      String(r.products?.pkgFiles).padEnd(8),
      String(r.products?.msPlaywrightLeaf).padEnd(7),
      String(r.products?.electronCacheLeaf).padEnd(6),
      String(r.catalogInForce),
    ].join(" "),
  );

// ⛔ VOID GATES, stated as gates rather than left to the reader.
const rigOff = results.find((r) => r.pkg === RIGCHECK && r.arm === "jailoff");
const rigOn = results.find((r) => r.pkg === RIGCHECK && r.arm === "shipped");
log("\n########## RIG CONTROL ##########");
log(`jail OFF  wrote outside the jail : ${rigOff?.rigcheckWrote} (target exists: ${rigOff?.rigcheckTargetExists})  <- MUST be true`);
log(`jail ON   refused                : ${rigOn?.rigcheckRefused} (target exists: ${rigOn?.rigcheckTargetExists})  <- MUST be refused/absent`);
const rigOk = rigOff?.rigcheckWrote === true && rigOn?.rigcheckWrote !== true;
log(`RIG CONTROL: ${rigOk ? "PASS — the rig can report failure" : "⛔ FAIL — every verdict in this run is VOID"}`);
const jailedVoid = results.filter((r) => r.arm !== "jailoff" && !r.error && (r.jaildumpLines || 0) === 0);
log(`jailed arms with ZERO JAILDUMP lines (VOID): ${jailedVoid.length}` + (jailedVoid.length ? " -> " + jailedVoid.map((r) => `${r.pkg}/${r.arm}`).join(", ") : ""));
const leafless = results.filter((r) => r.arm !== "jailoff" && (r.jaildumpLines || 0) > 0 && (r.leafRulesInPolicy || []).length < 3);
log(`jailed arms MISSING a leaf rule in the compiled policy: ${leafless.length}` + (leafless.length ? " -> " + leafless.map((r) => `${r.pkg}/${r.arm}:${JSON.stringify(r.leafRulesInPolicy)}`).join(", ") : ""));
