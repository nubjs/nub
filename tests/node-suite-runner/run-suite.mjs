// Node test-suite runner replicating tools/test.py semantics closely enough to
// compare runtimes on identical inputs.
//
// Fidelity decisions (each grounded in node-v26.3.0/tools/test.py + test/testpy):
//   - cwd = corpus root (test.py is invoked from the repo root; the test path is
//     passed absolute, and no chdir happens anywhere in the runner).
//   - NODE_SKIP_FLAG_CHECK is deliberately NOT set. test.py sets it because it
//     parses `// Flags:`/`// Env:` itself; by leaving it unset we let
//     test/common/index.js do the parse-and-respawn, which is the same behavior
//     without reimplementing the parser. Requires argv.length === 2, so the test
//     path is the only argument.
//   - TEST_THREAD_ID/TEST_SERIAL_ID are per-worker so common/tmpdir.js gives each
//     concurrent test its own .tmp.<id> directory (test.py:610-615).
//   - parallel/ runs concurrently, sequential/ serially, as upstream does.
//   - pass === exit code 0.

import { spawn } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";

const args = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = args.indexOf(`--${name}`);
  return i === -1 ? dflt : args[i + 1];
};
const has = (name) => args.includes(`--${name}`);

const RUNTIME = opt("runtime");            // path to binary
const RUNTIME_ARGS = (opt("runtime-args", "") || "").split(" ").filter(Boolean);
const LABEL = opt("label", path.basename(RUNTIME || "runtime"));
const CORPUS = path.resolve(opt("corpus"));  // .../node-v26.3.0
const LIST = path.resolve(opt("list"));      // newline-delimited test names
const OUT = path.resolve(opt("out"));
const JOBS = parseInt(opt("jobs", String(Math.max(2, os.cpus().length))), 10);
const TIMEOUT = parseInt(opt("timeout", "60"), 10) * 1000;
const LIMIT = parseInt(opt("limit", "0"), 10);
// Ambient NODE_OPTIONS is cleared by default (hygiene); an arm can set it
// deliberately to isolate a single flag's effect.
const NODE_OPTIONS = opt("node-options", "");

if (!RUNTIME || !CORPUS || !LIST || !OUT) {
  console.error("usage: --runtime <bin> --corpus <dir> --list <file> --out <json> [--runtime-args ..] [--jobs N] [--timeout S] [--limit N]");
  process.exit(2);
}

const TESTDIR = path.join(CORPUS, "test");
const TMPROOT = path.join(path.dirname(OUT), `tmp-${LABEL.replace(/[^\w.-]/g, "_")}`);
fs.rmSync(TMPROOT, { recursive: true, force: true });
fs.mkdirSync(TMPROOT, { recursive: true });

// ---- resolve the list against the real corpus -------------------------------
const raw = fs.readFileSync(LIST, "utf8").trim().split("\n").map((s) => s.trim()).filter(Boolean);
const resolved = [];
const unresolved = [];
for (const name of raw) {
  if (name.startsWith("js-native-api/") || name.startsWith("node-api/")) {
    unresolved.push({ name, why: "napi-addon" });   // needs a compiled binding; handled separately
    continue;
  }
  // A dir-prefixed name (es-module/foo.mjs) resolves literally; a bare name is
  // parallel-or-sequential. Both shapes are accepted so a list written against
  // another tracker's naming works unchanged alongside our own dir-scoped list.
  if (name.includes("/")) {
    const f = path.join(TESTDIR, name);
    if (fs.existsSync(f)) resolved.push({ name, dir: name.split("/")[0], file: f });
    else unresolved.push({ name, why: "absent-from-release-tarball" });
    continue;
  }
  const p = path.join(TESTDIR, "parallel", name);
  const s = path.join(TESTDIR, "sequential", name);
  if (fs.existsSync(p)) resolved.push({ name, dir: "parallel", file: p });
  else if (fs.existsSync(s)) resolved.push({ name, dir: "sequential", file: s });
  else unresolved.push({ name, why: "absent-from-release-tarball" });
}

// Incremental checkpoint: a crash mid-run must not lose the work, and must
// tell us exactly which test it died on.
const CKPT = OUT.replace(/\.json$/, "") + ".jsonl";
const already = new Map();
if (has("resume") && fs.existsSync(CKPT)) {
  for (const line of fs.readFileSync(CKPT, "utf8").split("\n")) {
    if (!line.trim()) continue;
    try { const r = JSON.parse(line); already.set(r.name, r); } catch {}
  }
  console.error(`[${LABEL}] resuming: ${already.size} results already recorded`);
} else {
  fs.writeFileSync(CKPT, "");
}
const ckpt = fs.createWriteStream(CKPT, { flags: "a" });

let work = resolved.filter((t) => !already.has(t.name));
if (LIMIT > 0) work = work.slice(0, LIMIT);
// Only sequential/ is forced serial upstream; every other dir is parallel-safe.
// Filtering for dir==="parallel" instead would silently drop es-module/ and
// module-hooks/ from BOTH queues -- they would never run and never be counted.
const sequentialTests = work.filter((t) => t.dir === "sequential");
const parallelTests = work.filter((t) => t.dir !== "sequential");

console.error(`[${LABEL}] ${work.length} runnable (${parallelTests.length} parallel, ${sequentialTests.length} sequential), ${unresolved.length} unresolved, jobs=${JOBS}, timeout=${TIMEOUT / 1000}s`);

// ---- run one test -----------------------------------------------------------
function runOne(test, threadId) {
  return new Promise((resolve) => {
    const started = Date.now();
    const child = spawn(RUNTIME, [...RUNTIME_ARGS, test.file], {
      cwd: CORPUS,
      // detached makes the child a process-group leader, so the timeout path can
      // kill(-pid) its whole tree. Without it, kill(-pid) signals whatever group
      // happens to own that id -- which can be OUR own group. That killed a run.
      detached: true,
      env: {
        ...process.env,
        TEST_SERIAL_ID: String(threadId),
        TEST_THREAD_ID: String(threadId),
        TEST_PARALLEL: String(JOBS),
        GITHUB_STEP_SUMMARY: "",
        NODE_TEST_DIR: TMPROOT,
        NODE_OPTIONS,
      },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let out = "", err = "", done = false;
    const cap = (s) => (s.length > 4000 ? s.slice(0, 4000) : s);
    child.stdout.on("data", (d) => { if (out.length < 8000) out += d; });
    child.stderr.on("data", (d) => { if (err.length < 8000) err += d; });

    const timer = setTimeout(() => {
      if (done) return;
      done = true;
      try { process.kill(-child.pid, "SIGKILL"); } catch {}
      try { child.kill("SIGKILL"); } catch {}
      resolve({ name: test.name, dir: test.dir, status: "timeout", code: null, ms: Date.now() - started, stderr: cap(err) });
    }, TIMEOUT);

    child.on("error", (e) => {
      if (done) return; done = true; clearTimeout(timer);
      resolve({ name: test.name, dir: test.dir, status: "spawn-error", code: null, ms: Date.now() - started, stderr: String(e) });
    });
    child.on("close", (code, signal) => {
      if (done) return; done = true; clearTimeout(timer);
      resolve({
        name: test.name, dir: test.dir,
        status: code === 0 ? "pass" : "fail",
        code, signal: signal || null,
        ms: Date.now() - started,
        stderr: code === 0 ? "" : cap(err || out),
      });
    });
  });
}

// ---- drive ------------------------------------------------------------------
const results = [...already.values()];
let idx = 0, completed = 0;
const total = work.length;
function progress() {
  if (completed % 250 === 0 || completed === total) {
    const p = results.filter((r) => r.status === "pass").length;
    console.error(`[${LABEL}] ${completed}/${total}  pass=${p}`);
  }
}

async function worker(threadId, queue) {
  for (;;) {
    const i = idx++;
    if (i >= queue.length) return;
    const r = await runOne(queue[i], threadId);
    ckpt.write(JSON.stringify(r) + "\n");
    results.push(r);
    completed++;
    progress();
  }
}

const t0 = Date.now();
idx = 0;
await Promise.all(Array.from({ length: JOBS }, (_, i) => worker(i, parallelTests)));
idx = 0;
await worker(0, sequentialTests);

const pass = results.filter((r) => r.status === "pass").length;
const summary = {
  label: LABEL,
  runtime: RUNTIME,
  runtimeArgs: RUNTIME_ARGS,
  nodeOptions: NODE_OPTIONS,
  corpus: CORPUS,
  listSize: raw.length,
  runnable: results.length,
  unresolved: unresolved.length,
  unresolvedBreakdown: unresolved.reduce((a, u) => ((a[u.why] = (a[u.why] || 0) + 1), a), {}),
  pass,
  fail: results.filter((r) => r.status === "fail").length,
  timeout: results.filter((r) => r.status === "timeout").length,
  spawnError: results.filter((r) => r.status === "spawn-error").length,
  pctOfRunnable: +((pass / results.length) * 100).toFixed(2),
  // % of every name in the list, including ones this environment cannot run --
  // the figure to use when comparing against a published tracker's denominator.
  pctOfListed: +((pass / raw.length) * 100).toFixed(2),
  wallSeconds: Math.round((Date.now() - t0) / 1000),
};
fs.writeFileSync(OUT, JSON.stringify({ summary, results, unresolved }, null, 2));
fs.rmSync(TMPROOT, { recursive: true, force: true });
console.error(`[${LABEL}] DONE ${JSON.stringify(summary, null, 2)}`);
