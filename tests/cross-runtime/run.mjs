#!/usr/bin/env node
// Cross-runtime Node-compatibility harness.
//
// Runs Node's own test suite — the `test/` tree of a Node checkout at one
// release (Deno's denoland/node_test vendors the same tree) — IDENTICALLY
// against node, nub, bun, and deno, and reports a non-cherry-picked pass rate
// per runtime under three scoring lenses:
//  - deno: Deno's directory set + Deno's config.jsonc skips (how Deno scores
//    itself on node-test-viewer);
//  - bun:  every test under parallel/ + sequential/, no skips (the universe
//    bun.com/node-test-suite draws its dots from, minus the js-native-api /
//    node-api addon tests, which need a compiled addon per test and are not
//    JS-executable);
//  - full: every JS-executable directory, no skips (the union of both, plus
//    async-hooks/ and report/, which neither lens counts).
// One run, one fixed file list, all lenses read off the same results.
//
// Faithfulness to Deno's runner (tests/node_compat/mod.rs):
//  - SAME corpus: all eligible files under runner/suite/test, applying Deno's
//    IGNORED_TEST_DIRS, then skipping config.jsonc `ignore:true` and
//    `<platform>:false` entries. (Darwin denominator reproduces Deno's own
//    4459 on the 25.8.1 corpus, matching the node-test-viewer Darwin snapshot.)
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
// Corpus root == Deno's `runner/suite` == a checkout of `denoland/node_test` at
// the commit named in README.md. For an outside reproduction, clone that commit
// and pass `--corpus <dir>`; without the flag we fall back to the local
// checkout under .repos/ (dev-box only).
function argValue(flag, dflt = null) {
  const i = process.argv.indexOf(flag);
  return i === -1 ? dflt : process.argv[i + 1];
}
const corpusArg = argValue("--corpus");
// Default: the tests/node-suite submodule — a full Node checkout at the pinned
// tag. A full checkout (not just test/) is what Node's own runner assumes:
// tests read doc/api, deps/npm and benchmark/ relative to the root.
const SUITE = corpusArg ? path.resolve(corpusArg) : path.join(REPO, "tests/node-suite"); // cwd for every test
const TEST_ROOT = path.join(SUITE, "test"); // file enumeration root
// The corpus names the Node version it vendors in node_version.ts.
// Deno's node_test shape carries node_version.ts; a Node checkout carries the
// version in src/node_version.h.
const CORPUS_NODE_VERSION = (() => {
  try {
    const m = /version = "([^"]+)"/.exec(fs.readFileSync(path.join(SUITE, "node_version.ts"), "utf8"));
    if (m) return m[1];
  } catch {}
  try {
    const h = fs.readFileSync(path.join(SUITE, "src/node_version.h"), "utf8");
    const n = (k) => h.match(new RegExp(`#define NODE_${k}_VERSION (\\d+)`))[1];
    return `${n("MAJOR")}.${n("MINOR")}.${n("PATCH")}`;
  } catch { return "?"; }
})();
const PTY_SPAWN = path.join(HERE, "pty-spawn.py");
// config.jsonc (Deno's skip list + per-test expected-failure config) lives in
// Deno's MAIN repo, NOT the corpus submodule — so it's vendored next to this
// harness for reproducibility. Prefer the vendored copy; fall back to the local
// deno checkout if the vendored file is absent.
const VENDORED_CONFIG = path.join(HERE, "config.jsonc");
const CONFIG_PATH = fs.existsSync(VENDORED_CONFIG) ? VENDORED_CONFIG : path.join(NODE_COMPAT, "config.jsonc");
const RESULTS_PATH = path.resolve(argValue("--out", path.join(HERE, "results.json")));

// Deno's IGNORED_TEST_DIRS (mod.rs). This set defines the `deno` lens.
const DENO_IGNORED_TEST_DIRS = new Set([
  "addons", "async-hooks", "benchmark", "cctest", "common", "doctool",
  "embedding", "fixtures", "fuzzers", "js-native-api", "known_issues",
  "node-api", "overlapped-checker", "report", "testpy", "tick-processor",
  "tools", "v8-updates", "wpt",
]);
// What we enumerate: Deno's set minus three directories of plain JS-executable
// tests Deno leaves out — async-hooks/ (the `async_hooks` API surface),
// report/ (`process.report`) and wpt/ (Node's wrappers over its in-tree Web
// Platform Tests subset, with Node's own expected-failure lists). Nothing is
// added to Deno's set, so the deno lens is exactly Deno's collection: ffi/
// (needs a compiled fixture — `npx node-gyp rebuild` in test/ffi/fixture_library)
// and test426/ stay in.
const FULL_IGNORED_TEST_DIRS = new Set(
  [...DENO_IGNORED_TEST_DIRS].filter((d) => !["async-hooks", "report", "wpt"].includes(d)),
);

// results.json is tracked, so nothing machine-specific goes into it: the
// corpus root, the repo root and the home directory are replaced in every
// captured output and binary path.
const HOME = os.homedir();
// The nub under test may live in another checkout (its preload frames name
// that checkout's runtime/ dir); scrub that root too.
let nubRoot; // resolved on first use: BINS is defined below
function scrub(s) {
  if (typeof s !== "string") return s;
  if (nubRoot === undefined) nubRoot = (/^(.*)\/target\/[^/]+\/nub$/.exec(path.resolve(BINS.nub)) || [])[1] || null;
  let out = s.split(SUITE).join("<corpus>").split(REPO).join("<repo>");
  if (nubRoot) out = out.split(nubRoot).join("<nub>");
  return out.split(HOME).join("~");
}
// Bun's tracker universe: parallel/ + sequential/ (+ the addon dirs we cannot run).
const BUN_LENS_DIRS = new Set(["parallel", "sequential"]);
// Tests that exercise the V8 engine or Node's private internals rather than
// Node's public API (engine-specific.txt, one corpus path per line; the rules
// are in README.md). They are NEVER dropped from a run — the *NoEngine lenses
// just score the corpus without them, for every runtime alike.
const ENGINE_SPECIFIC = (() => {
  try {
    return new Set(fs.readFileSync(path.join(HERE, "engine-specific.txt"), "utf8").split("\n").map((s) => s.trim()).filter((s) => s && !s.startsWith("#")));
  } catch { return new Set(); }
})();

const PLATFORM = (() => {
  const p = os.platform();
  if (p === "win32") return "windows";
  if (p === "darwin") return "darwin";
  return "linux";
})();

const TIMEOUT_MS = PLATFORM === "darwin" ? 20_000 : 10_000; // matches Deno
// A wpt/ wrapper runs hundreds of WPT files in one process (webcrypto: minutes).
const timeoutFor = (relPath) => (relPath.startsWith("wpt/") ? 300_000 : TIMEOUT_MS);
const PARALLELISM = parseInt(argValue("--parallelism", String(Math.max(4, Math.min(16, os.cpus().length)))), 10);
// Failures are re-run once at low parallelism, for EVERY runtime alike: on a
// loaded host a test that races the scheduler or a port can miss its own
// deadline, and a retry is what Deno's `flaky` handling does (up to 3 runs).
// Symmetric, so it cannot favour one runtime. `--no-retry` disables it.
const RETRY_FAILURES = !process.argv.includes("--no-retry");
const RETRY_PARALLELISM = 4;

// Runtime names map to a KIND (how the command is built) and a BINARY.
// `--runtimes node,nub,bun,deno,node25 --bin node25=/path/to/node` adds a
// second Node under its own label: any name starting with "node" is node-kind.
// Binaries default to PATH lookup except nub, which is the release build.
const BINS = {
  node: "node",
  nub: path.join(REPO, "target/release/nub"),
  bun: "bun",
  deno: "deno",
};
for (let i = 0; i < process.argv.length; i++) {
  if (process.argv[i] !== "--bin") continue;
  const [name, bin] = process.argv[i + 1].split("=");
  BINS[name] = bin;
}
function kindOf(runtime) {
  if (runtime === "nub") return "nub";
  if (runtime.startsWith("node")) return "node";
  if (runtime === "bun" || runtime === "deno") return runtime;
  throw new Error(`unknown runtime ${runtime}`);
}

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
    // Dot-entries are filtered at every level, not just inside walk(): a killed
    // test can leave a `.tmp.<pid>/` behind in the corpus, whose `test-*.js`
    // scratch files would otherwise enumerate as real tests.
    if (!top.isDirectory() || top.name.startsWith(".") || FULL_IGNORED_TEST_DIRS.has(top.name)) continue;
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
// `// Env: A=1 B=2` — Node's runner applies it to pseudo-tty tests.
function parseEnvDirective(source) {
  const m = /^\/\/ Env: (.+)$/m.exec(source);
  const env = {};
  if (m) for (const pair of m[1].trim().split(/\s+/)) { const [k, v] = pair.split("="); env[k] = v; }
  return env;
}

// test/pseudo-tty/ runs inside a pseudo-terminal, as in Node's own runner: the
// command is wrapped in pty-spawn.py, `// Env:` applies, and a sibling `.in`
// file (if any) is the test's stdin. Every runtime gets the same treatment.
function buildCommand(runtime, relPath, source, serialId) {
  const plain = buildPlainCommand(runtime, relPath, source, serialId);
  if (!relPath.startsWith("pseudo-tty/")) return plain;
  const inFile = path.join(TEST_ROOT, relPath.replace(/\.m?js$/, ".in"));
  // Node's runner inherits the ambient environment, which carries no NO_COLOR;
  // the harness sets NO_COLOR elsewhere to mirror Deno's runner, and these
  // tests assert exact warning text about colour env vars, so strip it here.
  const { NO_COLOR: _noColor, ...env } = plain.env;
  return {
    bin: "python3",
    args: [PTY_SPAWN, plain.bin, ...plain.args],
    env: { ...env, ...parseEnvDirective(source) },
    stdinFile: fs.existsSync(inFile) ? inFile : null,
  };
}

function buildPlainCommand(runtime, relPath, source, serialId) {
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

  const kind = kindOf(runtime);
  if (kind === "node" || kind === "nub") {
    const env = { ...baseEnv, NODE_OPTIONS: "", ...extraEnv };
    const args = [...nodeFlagArgs(tokens), testPath];
    return { bin: BINS[runtime], args, env };
  }

  if (kind === "bun") {
    // `bun <file>` == `bun run <file>`. Bun IGNORES NODE_OPTIONS entirely but
    // accepts Node-style flags as CLI arguments before the file — it honours the
    // ones it implements (`--expose-gc` defines `gc`), surfaces every token in
    // `process.execArgv`, and exits 0 on an unknown one (measured on bun 1.4.0
    // with --expose-gc, --no-warnings, --expose-internals, --max-old-space-size,
    // --allow-natives-syntax and a bogus flag). So bun gets the same `// Flags:`
    // tokens node gets, the same way. An earlier revision passed none, which
    // cost bun tests that only work with their flag (e.g. --expose-gc).
    const env = { ...baseEnv, NODE_OPTIONS: "", ...extraEnv };
    return { bin: BINS[runtime], args: [...nodeFlagArgs(tokens), testPath], env };
  }

  if (kind === "deno") {
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
    return { bin: BINS[runtime], args, env };
  }

  throw new Error(`unknown runtime ${runtime}`);
}

// ----- run a single (runtime, file) -----------------------------------------

function runOne(runtime, relPath, source, serialId) {
  const { bin, args, env, stdinFile } = buildCommand(runtime, relPath, source, serialId);
  const timeoutMs = timeoutFor(relPath);
  // pseudo-tty verdicts compare the whole output to a .out file, so keep it.
  const cap = relPath.startsWith("pseudo-tty/") ? 200_000 : 8000;
  return new Promise((resolve) => {
    let child;
    const stdinFd = stdinFile ? fs.openSync(stdinFile, "r") : null;
    try {
      child = spawn(bin, args, {
        cwd: SUITE,
        env: { PATH: process.env.PATH, HOME: process.env.HOME, ...env },
        stdio: [stdinFd ?? "ignore", "pipe", "pipe"],
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
    const take = (buf, which) => {
      const s = buf.toString();
      if (which === "o") out += out.length < cap ? s : "";
      else err += err.length < cap ? s : "";
    };
    child.stdout.on("data", (b) => take(b, "o"));
    child.stderr.on("data", (b) => take(b, "e"));
    const closeStdin = () => { if (stdinFd !== null) { try { fs.closeSync(stdinFd); } catch {} } };
    const timer = setTimeout(() => {
      if (done) return;
      done = true;
      killGroup("SIGKILL");
      closeStdin();
      resolve({ exit: null, timedOut: true, out: `Test timed out after ${timeoutMs}ms` });
    }, timeoutMs);
    child.on("error", (e) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      closeStdin();
      resolve({ exit: null, timedOut: false, out: String(e), spawnError: true });
    });
    child.on("close", (code, signal) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      closeStdin();
      // Reap any grandchildren the test left running even on a clean leader exit.
      killGroup("SIGKILL");
      const exit = code === null ? null : code;
      resolve({ exit, timedOut: false, out: `${out}\n${err}` });
    });
  });
}

// Apply Deno's pass criterion (incl. expected-failure handling).
// Node's pseudo-tty/testcfg.py verdict: the test's output, line by line, must
// match the sibling .out file — each line a pattern where `*` is `.*` and
// `%(basename)s` is the test's file name; blank lines and valgrind noise are
// ignored; the exit code is not consulted (some of these tests crash on
// purpose). Returns null when the test has no .out file.
const escapeRe = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
function judgePty(relPath, raw) {
  const outFile = path.join(TEST_ROOT, relPath.replace(/\.m?js$/, ".out"));
  if (!fs.existsSync(outFile)) return null;
  const basename = path.basename(relPath);
  const patterns = fs.readFileSync(outFile, "utf8").split("\n").filter((l) => l.trim()).map((l) =>
    new RegExp("^" + l.replace(/\s+$/, "").replace(/%\(basename\)s/g, basename).split("*").map(escapeRe).join(".*") + "$"));
  const lines = raw.out.split("\n").map((l) => l.replace(/\s+$/, "")).filter((l) => l.trim() && !l.startsWith("==") && !l.startsWith("**"));
  // Scrub BEFORE truncating: a cut that bisects an absolute path would leave a
  // suffix no later substring replacement can recognise.
  if (raw.timedOut) return { pass: false, timeout: true, exit: raw.exit, tail: scrub(raw.out.trim()).slice(-400) };
  if (lines.length !== patterns.length) return { pass: false, timeout: false, exit: raw.exit, tail: `expected ${patterns.length} output lines, got ${lines.length}\n` + scrub(raw.out.trim()).slice(-300) };
  for (let i = 0; i < patterns.length; i++) {
    if (!patterns[i].test(lines[i])) return { pass: false, timeout: false, exit: raw.exit, tail: `line ${i + 1} did not match ${patterns[i]}\n${lines[i]}` };
  }
  return { pass: true, timeout: false, exit: raw.exit, ptyOutput: true };
}

function judge(relPath, raw) {
  if (relPath.startsWith("pseudo-tty/")) {
    const j = judgePty(relPath, raw);
    if (j) return j;
  }
  const ef = resolveExpectedFailure(config[relPath]);
  const success = raw.exit === 0;
  if (!ef) {
    // A failure keeps the tail of its output so the record can be triaged
    // without re-running it — scrubbed of machine paths before the cut.
    return success
      ? { pass: true, timeout: false, exit: 0 }
      : { pass: false, timeout: raw.timedOut, exit: raw.exit, tail: scrub(raw.out.trim()).slice(-400) };
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

  if (RETRY_FAILURES) {
    // One retry per failed (file, runtime), low parallelism, every runtime
    // alike. The first attempt is kept on the record so a flipped verdict is
    // visible as such.
    const queue = [];
    for (const f of runList) for (const rt of runtimes) if (results[f][rt] && !results[f][rt].pass) queue.push([f, rt]);
    process.stderr.write(`Retrying ${queue.length} failed (file, runtime) pairs at parallelism ${RETRY_PARALLELISM}...\n`);
    let qi = 0, flipped = 0, retried = 0;
    async function retryWorker() {
      while (true) {
        const i = qi++;
        if (i >= queue.length) return;
        const [f, rt] = queue[i];
        const raw = await runOne(rt, f, sources[f], serial++);
        const j = judge(f, raw);
        retried++;
        if (j.pass) { flipped++; results[f][rt] = { ...j, retried: true, firstAttempt: results[f][rt] }; }
        else results[f][rt] = { ...results[f][rt], retried: true };
        if (retried % 100 === 0 || retried === queue.length) {
          process.stderr.write(`\r[retry] ${retried}/${queue.length}  flipped to pass: ${flipped}        `);
        }
      }
    }
    await Promise.all(Array.from({ length: RETRY_PARALLELISM }, () => retryWorker()));
    process.stderr.write("\n");
  }
  return results;
}

// ----- main ------------------------------------------------------------------

async function main() {
  const onlyRuntimes = argValue("--runtimes", "node,nub,bun,deno").split(",");
  for (const rt of onlyRuntimes) kindOf(rt);
  const limit = parseInt(argValue("--limit", "Infinity"), 10) || Infinity;
  // `--only <substring>` restricts the file list (smoke runs); `--dirs a,b`
  // restricts to top-level directories.
  const only = argValue("--only");
  const dirs = argValue("--dirs") ? new Set(argValue("--dirs").split(",")) : null;

  // Deno's config excludes 393 tests for Deno's own reasons. `--include-excluded`
  // runs them too, so ONE pass yields both the Deno-convention figure and the
  // full-corpus figure instead of two runs that could drift apart.
  const includeExcluded = process.argv.includes("--include-excluded");

  const eligible = enumerateEligible();
  const { run, ignored } = partition(eligible);
  const excludedSet = new Set(ignored.map((i) => i.path));
  let runList = includeExcluded ? eligible : run;
  if (only) runList = runList.filter((f) => f.includes(only));
  if (dirs) runList = runList.filter((f) => dirs.has(f.split("/")[0]));
  // `--files <list>` restricts to the newline-separated paths in a file;
  // `--merge <results.json>` re-runs that subset and folds the verdicts into a
  // prior results file (re-verifying failures on a quieter host, or
  // re-measuring one runtime after a binary change) — every score is then
  // recomputed from the merged record, so a merged file is never a hand edit.
  const filesArg = argValue("--files");
  if (filesArg) {
    const want = new Set(fs.readFileSync(filesArg, "utf8").split("\n").map((s) => s.trim()).filter(Boolean));
    runList = runList.filter((f) => want.has(f));
  }
  const mergeArg = argValue("--merge");
  const prior = mergeArg ? JSON.parse(fs.readFileSync(mergeArg, "utf8")) : null;
  if (Number.isFinite(limit)) runList = runList.slice(0, limit);

  // Preload sources once.
  const sources = {};
  for (const f of runList) {
    try { sources[f] = fs.readFileSync(path.join(TEST_ROOT, f), "utf8"); }
    catch { sources[f] = ""; }
  }

  const binaries = {};
  for (const rt of onlyRuntimes) {
    const kind = kindOf(rt);
    const entry = { bin: scrub(BINS[rt]), version: capture(`"${BINS[rt]}" --version`) };
    // nub augments whatever Node it resolves in the corpus cwd; record it.
    if (kind === "nub") entry.node = capture(`"${BINS[rt]}" -p process.version`, SUITE);
    binaries[rt] = entry;
  }
  process.stderr.write(
    `Corpus: ${SUITE} (Node v${CORPUS_NODE_VERSION}) | platform=${PLATFORM} | eligible=${eligible.length} | deno-ignored/skipped=${ignored.length} | running=${runList.length}\n` +
    `Runtimes: ${onlyRuntimes.join(", ")} | parallelism=${PARALLELISM} | timeout=${TIMEOUT_MS}ms | retry=${RETRY_FAILURES}\n` +
    `Binaries: ${JSON.stringify(binaries)}\n\n`,
  );

  let results = await runAll(onlyRuntimes, runList, sources);

  if (prior) {
    // Overlay the fresh verdicts on the prior record; anything not re-run keeps
    // its prior verdict, and the prior runtimes/binaries carry over.
    const merged = prior.results;
    for (const f of runList) merged[f] = { ...(merged[f] || {}), ...results[f] };
    results = merged;
    runList = Object.keys(merged).sort();
    for (const rt of Object.keys(prior.meta.binaries || {})) {
      if (!onlyRuntimes.includes(rt)) { onlyRuntimes.push(rt); binaries[rt] = prior.meta.binaries[rt]; }
    }
    // Every runtime must have a verdict for every file, or a lens would divide
    // a partial numerator by the full denominator. A new runtime can only be
    // merged in after a run over the whole list.
    for (const rt of onlyRuntimes) {
      const missing = runList.filter((f) => !results[f][rt]);
      if (missing.length) throw new Error(`--merge: runtime ${rt} has no verdict for ${missing.length} files (e.g. ${missing[0]}); run it over the full list first`);
    }
    process.stderr.write(`Merged into ${mergeArg}: ${runList.length} files, runtimes ${onlyRuntimes.join(", ")}\n`);
  }

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

  // node-fails are corpus/version artifacts. Run node + nub on Node v25.8.1 (the
  // corpus version) so node and binary align — a newer Node (26+) introduces
  // version-skew failures in the compat suite that aren't nub's concern.
  const nodeFailSet = new Set(fails.node || []);

  const denom = runList.length;
  const perRuntime = onlyRuntimes.map((rt) => ({
    runtime: rt,
    pass: summary[rt].pass,
    fail: summary[rt].fail,
    timeout: summary[rt].timeout,
    pct: +((summary[rt].pass / denom) * 100).toFixed(2),
  }));

  // Node-relative score over an arbitrary file subset: what share of the tests
  // real Node passes does this runtime also pass?
  //
  // Both halves MUST come from the same set. Scoring a runtime's total passes
  // against node's pass COUNT silently admits tests the runtime passes and node
  // fails, so the published "x / y" reads as a fraction of one set while its
  // numerator is drawn from a larger one. Intersecting fixes that.
  //
  // `rawPct` is the other reading — pass / files in the subset, no reference
  // runtime — which is how bun.com/node-test-suite scores (dots passing over
  // dots total). Both are reported; they answer different questions.
  function nodeRelative(subset) {
    const nodePassed = subset.filter((f) => results[f].node?.pass);
    return {
      files: subset.length,
      nodePass: nodePassed.length,
      runtimes: onlyRuntimes.map((rt) => {
        const pass = nodePassed.filter((f) => results[f][rt]?.pass).length;
        const rawPass = subset.filter((f) => results[f][rt]?.pass).length;
        return {
          runtime: rt,
          pass,
          pct: nodePassed.length ? +((pass / nodePassed.length) * 100).toFixed(2) : 0,
          rawPass,
          rawPct: subset.length ? +((rawPass / subset.length) * 100).toFixed(2) : 0,
        };
      }),
    };
  }

  const dirOf = (f) => f.split("/")[0];
  const denoConvention = runList.filter((f) => !excludedSet.has(f) && !DENO_IGNORED_TEST_DIRS.has(dirOf(f)));
  const bunUniverse = runList.filter((f) => BUN_LENS_DIRS.has(dirOf(f)));
  const excludedOnly = runList.filter((f) => excludedSet.has(f));
  const notEngine = (f) => !ENGINE_SPECIFIC.has(f);
  const scores = {
    // Deno's own directory set and config skips applied, as Deno scores itself.
    denoExclusions: nodeRelative(denoConvention),
    // parallel/ + sequential/, nothing skipped: the bun.com tracker's universe.
    bunUniverse: nodeRelative(bunUniverse),
    // Every enumerated file, nothing skipped.
    fullCorpus: nodeRelative(runList),
    // The same two, minus the engine-specific class (engine-specific.txt) —
    // symmetric for every runtime, see README.
    fullCorpusNoEngine: nodeRelative(runList.filter(notEngine)),
    bunUniverseNoEngine: nodeRelative(bunUniverse.filter(notEngine)),
    engineSpecificOnly: nodeRelative(runList.filter((f) => ENGINE_SPECIFIC.has(f))),
    // Deno's skipped tests alone (present only under --include-excluded).
    excludedOnly: excludedOnly.length ? nodeRelative(excludedOnly) : null,
    // Per top-level directory, full corpus — where each runtime loses tests.
    perDirectory: Object.fromEntries(
      [...new Set(runList.map(dirOf))].sort().map((d) => [d, nodeRelative(runList.filter((f) => dirOf(f) === d))]),
    ),
  };

  // nub-vs-node delta: files nub fails that node passes (real nub regressions)
  // and files node fails that nub passes (nub augmentation fixing a version-drift fail).
  const nubFails = new Set(fails.nub || []);
  const nubRegressions = (fails.nub || []).filter((f) => !nodeFailSet.has(f));
  const nubFixesVsNode = (fails.node || []).filter((f) => !nubFails.has(f));

  for (const f of runList) {
    for (const rt of Object.keys(results[f])) {
      const v = results[f][rt];
      if (v.tail) v.tail = scrub(v.tail);
      if (v.firstAttempt?.tail) v.firstAttempt.tail = scrub(v.firstAttempt.tail);
    }
  }

  const out = {
    meta: {
      generatedAt: new Date().toISOString(),
      platform: PLATFORM,
      corpus: scrub(SUITE),
      corpusNodeVersion: CORPUS_NODE_VERSION,
      // Only meaningful when the corpus dir is its own checkout; a tree produced
      // by `git archive` has no .git, and `git` would otherwise answer from the
      // enclosing repo.
      corpusCommit: fs.existsSync(path.join(SUITE, ".git")) ? capture("git rev-parse HEAD", SUITE) : null,
      binaries,
      eligibleFiles: eligible.length,
      ignoredOrSkipped: ignored.length,
      includeExcluded,
      denominator: denom,
      timeoutMs: TIMEOUT_MS,
      // Provenance of the bulk of the verdicts: a --merge run keeps the prior
      // file's parallelism and records its own separately.
      parallelism: prior ? prior.meta.parallelism : PARALLELISM,
      mergeParallelism: prior ? PARALLELISM : undefined,
      retryFailures: prior ? prior.meta.retryFailures : RETRY_FAILURES,
      mergeRetryFailures: prior ? RETRY_FAILURES : undefined,
      retried: Object.fromEntries(onlyRuntimes.map((rt) => [rt, {
        retried: runList.filter((f) => results[f][rt]?.retried).length,
        flippedToPass: runList.filter((f) => results[f][rt]?.retried && results[f][rt].pass).length,
      }])),
    },
    perRuntime,
    scores,
    nubVsNode: onlyRuntimes.includes("nub") && onlyRuntimes.includes("node") ? {
      nodeFailCount: nodeFailSet.size,
      nubRegressions, // nub fails, node passes => REAL nub compat bug
      nubFixesVsNode, // node fails (version drift), nub passes
    } : null,
    fails,
    ignored,
    results,
  };
  fs.writeFileSync(RESULTS_PATH, JSON.stringify(out, null, 2));

  // Print summary table.
  process.stderr.write("\n=== CROSS-RUNTIME NODE-COMPAT SUMMARY ===\n");
  process.stderr.write(`Corpus: Node v${CORPUS_NODE_VERSION} test/ tree | denominator=${denom} (${PLATFORM})\n`);
  process.stderr.write(`runtime    pass    fail  timeout    pct\n`);
  for (const r of perRuntime) {
    process.stderr.write(
      `${r.runtime.padEnd(8)} ${String(r.pass).padStart(6)} ${String(r.fail).padStart(6)} ${String(r.timeout).padStart(8)}  ${r.pct.toFixed(2)}%\n`,
    );
  }
  const printScore = (label, s) => {
    if (!s) return;
    process.stderr.write(`\n${label} — ${s.files} files, node passes ${s.nodePass}\n`);
    process.stderr.write(`runtime   node-relative          raw\n`);
    for (const r of s.runtimes) {
      process.stderr.write(`${r.runtime.padEnd(8)} ${String(r.pass).padStart(6)} / ${s.nodePass}  ${r.pct.toFixed(2).padStart(6)}%   ${String(r.rawPass).padStart(6)} / ${s.files}  ${r.rawPct.toFixed(2).padStart(6)}%\n`);
    }
  };
  process.stderr.write("\n=== LENSES (node-relative: numerator and denominator from the same set; raw: pass / files) ===");
  printScore("deno lens — Deno's directories + config skips", scores.denoExclusions);
  printScore("bun lens — parallel/ + sequential/, no skips", scores.bunUniverse);
  printScore("full corpus, no skips", scores.fullCorpus);
  printScore("full corpus minus the engine-specific class", scores.fullCorpusNoEngine);
  printScore("bun lens minus the engine-specific class", scores.bunUniverseNoEngine);
  printScore("the engine-specific class alone", scores.engineSpecificOnly);
  printScore("Deno's skipped tests alone", scores.excludedOnly);

  if (out.nubVsNode) {
    process.stderr.write(`\nnub regressions vs node (nub fails, node passes): ${nubRegressions.length}\n`);
    process.stderr.write(`node fails that nub passes (version-drift muted by nub): ${nubFixesVsNode.length}\n`);
  }
  process.stderr.write(`\nResults written to ${RESULTS_PATH}\n`);
}

import { execSync } from "node:child_process";
function capture(cmd, cwd) {
  try { return execSync(cmd, { encoding: "utf8", cwd, stdio: ["ignore", "pipe", "ignore"] }).trim(); } catch { return "?"; }
}

main().catch((e) => { console.error(e); process.exit(1); });
