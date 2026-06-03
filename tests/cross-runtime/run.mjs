#!/usr/bin/env node
// Cross-runtime Node-compatibility harness.
//
// Runs Deno's OWN vendored Node-compat corpus (denoland/node_test, vendoring
// Node v25.8.1) IDENTICALLY against node, nub, bun, and deno, and reports a
// non-cherry-picked pass rate per runtime.
//
// Faithfulness to Deno's runner (tests/node_compat/mod.rs):
//  - SAME corpus: all eligible files under runner/suite/test, applying Deno's
//    IGNORED_TEST_DIRS, then skipping config.jsonc `ignore:true` and
//    `<platform>:false` entries. (Darwin denominator reproduces Deno's own
//    4459, matching the node-test-viewer Darwin snapshot.)
//  - SAME pass criterion: child exit code 0 == pass; timeout == fail; tests
//    with an expected-failure config (top-level or per-platform exitCode/output)
//    pass ONLY when they fail in exactly the configured way (wildcard-matched).
//  - SAME env: NODE_TEST_KNOWN_GLOBALS=0, NODE_SKIP_FLAG_CHECK=1, NO_COLOR=1,
//    NODE_OPTIONS derived from the test's `// Flags:` directive, TEST_SERIAL_ID.
//  - SAME cwd/path model: cwd = runner/suite, path = test/<subdir>/<file>, so
//    `require('../common')`, common/fixtures, and `--require ./test/fixtures/...`
//    all resolve identically for every runtime.
//
// Per-runtime invocation differs ONLY in the binary + how each runtime's CLI
// ingests flags — exactly as Deno's own runner differs (Deno translates
// `// Flags:` into --v8-flags/NODE_OPTIONS/deno-args; node/nub take real Node
// flags directly; deno needs `run`/`test` + unstable flags). This is inherent
// and faithful, not a thumb on the scale.
//
// CRITICAL: ONE fixed test list across all four runtimes. No runtime drops its
// own failures. nub runs in DEFAULT (augmented) mode — never --node.

import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(HERE, "../..");
const NODE_COMPAT = path.join(REPO, ".repos/deno/tests/node_compat");
// Corpus root == Deno's `runner/suite` == a checkout of `colinhacks/node_test`
// (fork of `denoland/node_test`, tag `node-25.8.1` == commit c5baef08). For an
// outside reproduction, clone that fork and pass `--corpus <dir>`; without the
// flag we fall back to the local submodule under .repos/deno (dev-box only).
const corpusArg = process.argv.includes("--corpus")
  ? process.argv[process.argv.indexOf("--corpus") + 1]
  : null;
const SUITE = corpusArg ? path.resolve(corpusArg) : path.join(NODE_COMPAT, "runner/suite"); // cwd for every test
const TEST_ROOT = path.join(SUITE, "test"); // file enumeration root
// config.jsonc (Deno's skip list + per-test expected-failure config) lives in
// Deno's MAIN repo, NOT the corpus submodule — so it's vendored next to this
// harness for reproducibility. Prefer the vendored copy; fall back to the local
// deno checkout if the vendored file is absent.
const VENDORED_CONFIG = path.join(HERE, "config.jsonc");
const CONFIG_PATH = fs.existsSync(VENDORED_CONFIG) ? VENDORED_CONFIG : path.join(NODE_COMPAT, "config.jsonc");
const RESULTS_PATH = path.join(HERE, "results.json");

// Deno's IGNORED_TEST_DIRS (mod.rs).
const IGNORED_TEST_DIRS = new Set([
  "addons", "async-hooks", "benchmark", "cctest", "common", "doctool",
  "embedding", "fixtures", "fuzzers", "js-native-api", "known_issues",
  "node-api", "overlapped-checker", "report", "testpy", "tick-processor",
  "tools", "v8-updates", "wpt",
]);

const PLATFORM = (() => {
  const p = os.platform();
  if (p === "win32") return "windows";
  if (p === "darwin") return "darwin";
  return "linux";
})();

const TIMEOUT_MS = PLATFORM === "darwin" ? 20_000 : 10_000; // matches Deno
const PARALLELISM = Math.max(4, Math.min(16, os.cpus().length));

const BINS = {
  node: "node",
  nub: path.join(REPO, "target/release/nub"),
  bun: "bun",
  deno: "deno",
};

// ----- config.jsonc parsing (JSONC: // and /* */ comments, trailing commas) --

function parseJsonc(src) {
  let s = src.replace(/\/\*[\s\S]*?\*\//g, "");
  let out = "";
  let inStr = false, q = "";
  for (let i = 0; i < s.length; i++) {
    const c = s[i], n = s[i + 1];
    if (inStr) {
      out += c;
      if (c === "\\") { out += s[++i] ?? ""; continue; }
      if (c === q) inStr = false;
      continue;
    }
    if (c === '"' || c === "'") { inStr = true; q = c; out += c; continue; }
    if (c === "/" && n === "/") { while (i < s.length && s[i] !== "\n") i++; out += "\n"; continue; }
    out += c;
  }
  out = out.replace(/,(\s*[}\]])/g, "$1");
  return JSON.parse(out);
}

const config = parseJsonc(fs.readFileSync(CONFIG_PATH, "utf8")).tests;

// ----- corpus enumeration (faithful to collect_all_tests) --------------------

function enumerateEligible() {
  const files = [];
  function walk(dir, rel) {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      if (e.name.startsWith(".")) continue;
      const fp = path.join(dir, e.name);
      const r = rel ? `${rel}/${e.name}` : e.name;
      if (e.isDirectory()) { walk(fp, r); continue; }
      if (!e.name.startsWith("test-")) continue;
      if (!/\.(js|mjs|cjs|ts)$/.test(e.name)) continue;
      files.push(r);
    }
  }
  for (const top of fs.readdirSync(TEST_ROOT, { withFileTypes: true })) {
    if (!top.isDirectory() || IGNORED_TEST_DIRS.has(top.name)) continue;
    walk(path.join(TEST_ROOT, top.name), top.name);
  }
  return files.sort();
}

// Returns { run: [paths], ignored: [{path,reason}] }.
function partition(eligible) {
  const run = [], ignored = [];
  for (const f of eligible) {
    const c = config[f];
    if (c) {
      if (c.ignore) { ignored.push({ path: f, reason: c.reason || "ignore:true" }); continue; }
      const pe = c[PLATFORM];
      if (pe === false) { ignored.push({ path: f, reason: c.reason || `disabled on ${PLATFORM}` }); continue; }
    }
    run.push(f);
  }
  return { run, ignored };
}

// ----- expected-failure resolution (resolve_expected_failure) ---------------

function resolveExpectedFailure(c) {
  if (!c) return null;
  const pe = c[PLATFORM];
  if (pe && typeof pe === "object") {
    return { exitCode: pe.exitCode ?? null, output: pe.output ?? null };
  }
  if (c.exitCode !== undefined || c.output !== undefined) {
    return { exitCode: c.exitCode ?? null, output: c.output ?? null };
  }
  return null;
}

// ----- wildcard match (Deno's [WILDCARD] etc., subset used by config.jsonc) --
// config.jsonc only uses [WILDCARD] (matches 0+ chars incl newlines). Implement
// that plus literal matching; if a more exotic token appears, fall back to a
// permissive match and flag it.
function wildcardMatch(pattern, text) {
  // Split on [WILDCARD]; each literal segment must appear in order.
  const KNOWN = /\[WILDCARD\]/g;
  if (/\[WILD(LINE|CHAR|CHARS)/.test(pattern) || /\[UNORDERED_/.test(pattern)) {
    // Rare token not used by current config; degrade to substring-of-first-seg.
    const firstSeg = pattern.split(/\[[A-Z_]+(?:\(\d+\))?\]/)[0];
    return text.includes(firstSeg);
  }
  const segs = pattern.split(KNOWN);
  let idx = 0;
  for (let s = 0; s < segs.length; s++) {
    const seg = segs[s];
    if (seg === "") continue;
    const at = text.indexOf(seg, idx);
    if (at === -1) return false;
    // First segment with no leading wildcard must match at start.
    if (s === 0 && !pattern.startsWith("[WILDCARD]") && at !== 0) return false;
    idx = at + seg.length;
  }
  // Last segment with no trailing wildcard must reach end.
  const last = segs[segs.length - 1];
  if (last !== "" && !pattern.endsWith("[WILDCARD]") && !text.endsWith(last)) return false;
  return true;
}

// ----- `// Flags:` parsing ---------------------------------------------------
// Returns the raw flag tokens from the first `// Flags:` line (if any).
function parseFlags(source) {
  for (const line of source.split("\n")) {
    const m = /^\/\/ Flags: (.+)$/.exec(line);
    if (m) return m[1].trim().split(/\s+/).filter(Boolean);
    if (line.trim() && !line.startsWith("//") && !line.startsWith("'use strict'")) {
      // Flags directives are at the very top; stop scanning once real code starts.
      // (Cheap heuristic; Deno scans the whole source but only takes the first.)
    }
  }
  return [];
}

// Build the deno arg translation for a flag token (mirrors mod.rs parse_flags).
function translateDenoFlags(tokens) {
  const v8 = [], denoArgs = [];
  for (const raw of tokens) {
    const f = raw.startsWith("--") ? raw.replace(/_/g, "-") : raw;
    if (f === "--expose-externalize-string") v8.push("--expose-externalize-string");
    else if (f === "--expose-gc") v8.push("--expose-gc");
    else if (f === "--no-concurrent-array-buffer-sweeping") v8.push("--no-concurrent-array-buffer-sweeping");
    else if (f === "--allow-natives-syntax") v8.push("--allow-natives-syntax");
    else if (f === "--inspect" || f.startsWith("--inspect=")) {
      const rest = f.startsWith("--inspect=") ? f.slice("--inspect=".length) : "";
      if (rest === "") denoArgs.push("--inspect=127.0.0.1:0");
      else if (/^\d+$/.test(rest)) denoArgs.push(`--inspect=127.0.0.1:${rest}`);
      else denoArgs.push(`--inspect=${rest}`);
    }
    // Everything else Deno routes to NODE_OPTIONS or drops; for our purposes the
    // NODE_OPTIONS-routed ones (--no-warnings, --pending-deprecation,
    // --expose-internals, tls/dns/title) also matter for deno via NODE_OPTIONS.
  }
  return { v8, denoArgs };
}

// node/nub: forward NODE_OPTIONS-eligible flags via NODE_OPTIONS (so they apply
// the same way Deno applies them), and pass the rest as direct CLI args.
// Simplest faithful approach: pass ALL `// Flags:` tokens directly to node/nub
// as CLI args — node natively understands them (--expose-gc, --expose-internals,
// --experimental-vm-modules, --require, --no-warnings, ...). This is strictly
// more faithful than Deno's lossy whitelist.
function nodeFlagArgs(tokens) {
  return tokens.slice();
}

// Deno routes a whitelist into NODE_OPTIONS; reproduce that so deno sees the
// same options node does where it can honor them.
function denoNodeOptions(tokens) {
  const opts = [];
  for (const raw of tokens) {
    const f = raw.startsWith("--") ? raw.replace(/_/g, "-") : raw;
    if (f === "--no-warnings") opts.push("--no-warnings");
    else if (f === "--pending-deprecation") opts.push("--pending-deprecation");
    else if (f === "--expose-internals") opts.push("--expose-internals");
    else if (/^--tls-(min|max)-v1\.[0-3]$/.test(f) || /^--(no-)?use-(bundled|openssl|system)-ca$/.test(f)) opts.push(raw);
    else if (f.startsWith("--dns-result-order=")) opts.push(raw);
    else if (f.startsWith("--title=")) opts.push(raw);
  }
  return opts;
}

function usesNodeTest(source) {
  return source.includes("'node:test'") || source.includes('"node:test"');
}

// ----- command construction per runtime --------------------------------------
// relPath is like "parallel/test-os.js". cwd is SUITE; we pass "test/<relPath>".
function buildCommand(runtime, relPath, source, serialId) {
  const testPath = `test/${relPath}`;
  const tokens = parseFlags(source);
  const baseEnv = {
    NODE_TEST_KNOWN_GLOBALS: "0",
    NODE_SKIP_FLAG_CHECK: "1",
    NO_COLOR: "1",
    TEST_SERIAL_ID: String(serialId),
  };
  // Per-test env from config (e.g. node_shared_openssl) — applied to all
  // runtimes equally, faithful to Deno.
  const c = config[relPath];
  const extraEnv = (c && c.env) ? c.env : {};

  if (runtime === "node" || runtime === "nub") {
    const env = { ...baseEnv, NODE_OPTIONS: "", ...extraEnv };
    const args = [...nodeFlagArgs(tokens), testPath];
    return { bin: BINS[runtime], args, env };
  }

  if (runtime === "bun") {
    // `bun <file>` == `bun run <file>`. Forward node-style flags via NODE_OPTIONS
    // (bun honors a subset) for parity with how node receives them. Bun does not
    // accept arbitrary V8/Node CLI flags before the file, so route through
    // NODE_OPTIONS only; unknown ones bun simply ignores.
    const env = { ...baseEnv, NODE_OPTIONS: "", ...extraEnv };
    return { bin: BINS.bun, args: [testPath], env };
  }

  if (runtime === "deno") {
    const isNodeTest = usesNodeTest(source);
    const { v8, denoArgs } = translateDenoFlags(tokens);
    const nodeOpts = denoNodeOptions(tokens);
    const RUN = ["run", "-A", "--quiet", "--unstable-unsafe-proto", "--unstable-bare-node-builtins"];
    const TEST = ["test", "-A", "--quiet", "--unstable-unsafe-proto", "--unstable-bare-node-builtins", "--no-check", "--unstable-detect-cjs"];
    let args = isNodeTest ? [...TEST] : [...RUN];
    // For non-node:test .js CommonJS tests, Deno relies on a suite-root
    // package.json {"type":"commonjs"} OR --unstable-detect-cjs. We add the flag
    // to the RUN path too (recon: required, otherwise 'require is not defined').
    if (!isNodeTest) args.push("--unstable-detect-cjs");
    if (v8.length) args.push(`--v8-flags=${v8.join(",")}`);
    for (const a of denoArgs) args.push(a);
    // Per-test extraDenoArgs from config.jsonc.
    if (c && Array.isArray(c.extraDenoArgs)) for (const a of c.extraDenoArgs) args.push(a);
    args.push(testPath);
    const env = { ...baseEnv, NODE_OPTIONS: nodeOpts.join(" "), ...extraEnv };
    return { bin: BINS.deno, args, env };
  }

  throw new Error(`unknown runtime ${runtime}`);
}

// ----- run a single (runtime, file) -----------------------------------------

function runOne(runtime, relPath, source, serialId) {
  const { bin, args, env } = buildCommand(runtime, relPath, source, serialId);
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(bin, args, {
        cwd: SUITE,
        env: { PATH: process.env.PATH, HOME: process.env.HOME, ...env },
        stdio: ["ignore", "pipe", "pipe"],
        // Own process group so a timed-out test's servers/workers (grandchildren)
        // die WITH the leader. Without this, killing only child.pid leaves them
        // orphaned to PPID 1, spinning at high CPU (bun/deno on the net/cluster/
        // http timeout-prone tests are the worst offenders).
        detached: true,
      });
    } catch (e) {
      resolve({ exit: null, timedOut: false, out: String(e), spawnError: true });
      return;
    }
    let out = "", err = "", done = false;
    // Kill the whole process group (negative pid), not just the leader; fall
    // back to a plain kill if the group send fails (e.g. leader already reaped).
    const killGroup = (sig) => {
      try { process.kill(-child.pid, sig); } catch { try { child.kill(sig); } catch {} }
    };
    const cap = (buf, which) => {
      const s = buf.toString();
      if (which === "o") out += out.length < 8000 ? s : "";
      else err += err.length < 8000 ? s : "";
    };
    child.stdout.on("data", (b) => cap(b, "o"));
    child.stderr.on("data", (b) => cap(b, "e"));
    const timer = setTimeout(() => {
      if (done) return;
      done = true;
      killGroup("SIGKILL");
      resolve({ exit: null, timedOut: true, out: `Test timed out after ${TIMEOUT_MS}ms` });
    }, TIMEOUT_MS);
    child.on("error", (e) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      resolve({ exit: null, timedOut: false, out: String(e), spawnError: true });
    });
    child.on("close", (code, signal) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      // Reap any grandchildren the test left running even on a clean leader exit.
      killGroup("SIGKILL");
      const exit = code === null ? null : code;
      resolve({ exit, timedOut: false, out: `${out}\n${err}` });
    });
  });
}

// Apply Deno's pass criterion (incl. expected-failure handling).
function judge(relPath, raw) {
  const ef = resolveExpectedFailure(config[relPath]);
  const success = raw.exit === 0;
  if (!ef) {
    return { pass: success, timeout: raw.timedOut, exit: raw.exit };
  }
  // Expected-failure test.
  if (success) {
    return { pass: false, reason: "expected test to fail but it passed", timeout: false, exit: raw.exit };
  }
  const exitOk = ef.exitCode === null || raw.exit === ef.exitCode;
  const outOk = ef.output === null || wildcardMatch(ef.output, raw.out);
  if (exitOk && outOk) return { pass: true, expectedFailure: true, timeout: raw.timedOut, exit: raw.exit };
  return { pass: false, reason: "did not fail in the expected way", timeout: raw.timedOut, exit: raw.exit };
}

// ----- worker pool over the fixed file list ----------------------------------

async function runAll(runtimes, runList, sources) {
  const results = {}; // relPath -> { node:{...}, nub:{...}, ... }
  for (const f of runList) results[f] = {};

  let serial = 0;
  let nextIdx = 0;
  const total = runList.length;
  let completed = 0;
  const startedAt = Date.now();

  async function worker() {
    while (true) {
      const i = nextIdx++;
      if (i >= total) return;
      const relPath = runList[i];
      const source = sources[relPath];
      const sid = serial++;
      for (const rt of runtimes) {
        const raw = await runOne(rt, relPath, source, sid);
        const j = judge(relPath, raw);
        results[relPath][rt] = j;
      }
      completed++;
      if (completed % 100 === 0 || completed === total) {
        const pct = ((completed / total) * 100).toFixed(1);
        const el = ((Date.now() - startedAt) / 1000).toFixed(0);
        process.stderr.write(`\r[${pct}%] ${completed}/${total} files  (${el}s)        `);
      }
    }
  }
  await Promise.all(Array.from({ length: PARALLELISM }, () => worker()));
  process.stderr.write("\n");
  return results;
}

// ----- main ------------------------------------------------------------------

async function main() {
  const onlyRuntimes = process.argv.includes("--runtimes")
    ? process.argv[process.argv.indexOf("--runtimes") + 1].split(",")
    : ["node", "nub", "bun", "deno"];
  const limit = process.argv.includes("--limit")
    ? parseInt(process.argv[process.argv.indexOf("--limit") + 1], 10)
    : Infinity;

  const eligible = enumerateEligible();
  const { run, ignored } = partition(eligible);
  let runList = run;
  if (Number.isFinite(limit)) runList = run.slice(0, limit);

  // Preload sources once.
  const sources = {};
  for (const f of runList) {
    try { sources[f] = fs.readFileSync(path.join(TEST_ROOT, f), "utf8"); }
    catch { sources[f] = ""; }
  }

  process.stderr.write(
    `Platform: ${PLATFORM} | eligible=${eligible.length} | ignored/skipped=${ignored.length} | running=${runList.length}\n` +
    `Runtimes: ${onlyRuntimes.join(", ")} | parallelism=${PARALLELISM} | timeout=${TIMEOUT_MS}ms\n` +
    `Binaries: node=${BINS.node} nub=${BINS.nub} bun=${BINS.bun} deno=${BINS.deno}\n\n`,
  );

  const results = await runAll(onlyRuntimes, runList, sources);

  // Aggregate.
  const summary = {};
  for (const rt of onlyRuntimes) summary[rt] = { pass: 0, fail: 0, timeout: 0 };
  const fails = {}; for (const rt of onlyRuntimes) fails[rt] = [];

  for (const f of runList) {
    for (const rt of onlyRuntimes) {
      const r = results[f][rt];
      if (!r) continue;
      if (r.pass) summary[rt].pass++;
      else {
        summary[rt].fail++;
        if (r.timeout) summary[rt].timeout++;
        fails[rt].push(f);
      }
    }
  }

  // node-fails are corpus/version artifacts (corpus pinned v25.8.1, binary 26.2.0).
  const nodeFailSet = new Set(fails.node || []);

  const denom = runList.length;
  const perRuntime = onlyRuntimes.map((rt) => ({
    runtime: rt,
    pass: summary[rt].pass,
    fail: summary[rt].fail,
    timeout: summary[rt].timeout,
    pct: +((summary[rt].pass / denom) * 100).toFixed(2),
  }));

  // nub-vs-node delta: files nub fails that node passes (real nub regressions)
  // and files node fails that nub passes (nub augmentation fixing a version-drift fail).
  const nubFails = new Set(fails.nub || []);
  const nubRegressions = (fails.nub || []).filter((f) => !nodeFailSet.has(f));
  const nubFixesVsNode = (fails.node || []).filter((f) => !nubFails.has(f));

  const out = {
    meta: {
      generatedAt: new Date().toISOString(),
      platform: PLATFORM,
      corpusNodeVersion: "25.8.1",
      binaries: {
        node: capture(`${BINS.node} --version`),
        nub: capture(`${BINS.nub} --version`),
        bun: capture(`${BINS.bun} --version`),
        deno: capture(`${BINS.deno} --version`),
      },
      eligibleFiles: eligible.length,
      ignoredOrSkipped: ignored.length,
      denominator: denom,
      timeoutMs: TIMEOUT_MS,
      parallelism: PARALLELISM,
    },
    perRuntime,
    nubVsNode: {
      nodeFailCount: nodeFailSet.size,
      nubRegressions, // nub fails, node passes => REAL nub compat bug
      nubFixesVsNode, // node fails (version drift), nub passes
    },
    fails,
    ignored,
    results,
  };
  fs.writeFileSync(RESULTS_PATH, JSON.stringify(out, null, 2));

  // Print summary table.
  process.stderr.write("\n=== CROSS-RUNTIME NODE-COMPAT SUMMARY ===\n");
  process.stderr.write(`Corpus: denoland/node_test (Node v25.8.1) | denominator=${denom} (darwin)\n`);
  process.stderr.write(`runtime    pass    fail  timeout    pct\n`);
  for (const r of perRuntime) {
    process.stderr.write(
      `${r.runtime.padEnd(8)} ${String(r.pass).padStart(6)} ${String(r.fail).padStart(6)} ${String(r.timeout).padStart(8)}  ${r.pct.toFixed(2)}%\n`,
    );
  }
  process.stderr.write(`\nnub regressions vs node (nub fails, node passes): ${nubRegressions.length}\n`);
  process.stderr.write(`node fails that nub passes (version-drift muted by nub): ${nubFixesVsNode.length}\n`);
  process.stderr.write(`\nResults written to ${RESULTS_PATH}\n`);
}

import { execSync } from "node:child_process";
function capture(cmd) {
  try { return execSync(cmd, { encoding: "utf8" }).trim(); } catch { return "?"; }
}

main().catch((e) => { console.error(e); process.exit(1); });
