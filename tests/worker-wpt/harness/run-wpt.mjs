// WPT Worker-conformance runner for nub.
//
// Drives a pinned, vendored slice of web-platform-tests `.any.js` files
// (webmessaging + the structured-clone battery + a couple of worker-scope tests)
// THROUGH nub's runtime, so the polyfilled globals under test — Worker,
// MessageChannel, MessagePort, MessageEvent, structuredClone — are nub's. It
// mirrors Node's `test/common/wpt.js` model: a vendored subset + a per-module
// status file (skip / expected-fail / pass) + a subprocess-per-file driver that
// loads the REAL `testharness.js`.
//
// Green = every subtest's outcome MATCHES the status file: a `skip` file is not
// run; an `expected-fail` subtest must fail; every other subtest must pass. An
// UNEXPECTED pass on an expected-fail (the divergence got fixed) is reported so
// the status file can be tightened, but does NOT fail the run. An UNEXPECTED fail
// (a regression) fails the run.
//
// Each file runs in its OWN nub subprocess (full per-file isolation + a real nub
// global install per file, exactly like Node's worker-thread-per-file model).
// The nub binary is resolved from $WPT_NUB, else the dev `target/fast/nub` next
// to this repo, else `nub` on PATH.
//
// Usage:
//   nub run-wpt.mjs                 # run the whole status-file matrix
//   node run-wpt.mjs                # same, if a nub binary is discoverable
//   WPT_NUB=/path/to/nub node run-wpt.mjs
//   node run-wpt.mjs --filter webmessaging   # only matching rel paths

import { readFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const HERE = dirname(fileURLToPath(import.meta.url));
const WPT_ROOT = resolve(HERE, "..", "wpt");
const STATUS_PATH = resolve(HERE, "..", "status.json");
const DRIVER = join(HERE, "run-file.mjs");

const PER_FILE_TIMEOUT_MS = Number(process.env.WPT_FILE_TIMEOUT_MS || 20000);

function findNub() {
  if (process.env.WPT_NUB) return process.env.WPT_NUB;
  // The dev binary placed next to a sibling runtime/ dir (worktree target/fast).
  const repoRoot = resolve(HERE, "..", "..", "..");
  for (const c of [
    join(repoRoot, "target", "fast", "nub"),
    join(repoRoot, "target", "release", "nub"),
  ]) {
    if (existsSync(c)) return c;
  }
  return "nub";
}

const NUB = findNub();
const status = JSON.parse(readFileSync(STATUS_PATH, "utf8"));

// The Node version under test. In CI each matrix leg runs `node run-wpt.mjs` with
// that leg's Node on PATH (setup-node), so this orchestrator's own process.version
// IS the version every subprocess must run under. We ENFORCE that below by pinning
// NODE_EXECUTABLE=process.execPath on the spawned nub children — without it, nub
// would read the repo-root package.json `engines.node` (>=22.15.0) and silently
// PROVISION a satisfying Node on the floor legs, so the "Node 18.19/20 leg" would
// actually run a >=22.15 Node and the floor coverage would be vacuous.
//
// NODE_MAJOR is used to gate VERSION-SPECIFIC expected-fails (a divergence inherited
// from a specific Node line, e.g. structuredClone(File) losing its File-ness on Node
// 22, fixed in Node 24+) so the gate stays green on the floor without masking the
// behavior on versions where it's actually correct. The NODE_EXECUTABLE pin keeps
// the children's Node provably equal to this NODE_MAJOR.
const NODE_MAJOR = Number(process.versions.node.split(".")[0]) || 0;

// Resolve a fail-entry to the set of subtest names expected to fail ON THIS Node.
// Plain `fail.expected` applies to ALL versions; `fail.versioned` is a list of
// { maxMajor?, minMajor?, expected:[…], note } whose `expected` names apply only
// when NODE_MAJOR falls in [minMajor, maxMajor] (either bound optional/inclusive).
function expectedFailsFor(entry) {
  const names = new Set((entry.fail && entry.fail.expected) || []);
  for (const v of (entry.fail && entry.fail.versioned) || []) {
    const okMin = v.minMajor == null || NODE_MAJOR >= v.minMajor;
    const okMax = v.maxMajor == null || NODE_MAJOR <= v.maxMajor;
    if (okMin && okMax) for (const n of v.expected) names.add(n);
  }
  return names;
}

const args = process.argv.slice(2);
const filterIdx = args.indexOf("--filter");
const filter = filterIdx >= 0 ? args[filterIdx + 1] : null;

// A status entry per file:
//   { skip: "<reason>" }                         → not run
//   { fail: { expected: ["<subtest name>", …], note: "<why documented>" } }
//   {}                                            → all subtests must pass
// A file may also declare `scopes: ["window","worker"]` to run both passes; the
// default scope set is derived from the file's META global= directive below.
const files = Object.keys(status.tests).filter((rel) => !filter || rel.includes(filter));

function metaGlobals(rel) {
  const src = readFileSync(join(WPT_ROOT, rel), "utf8");
  for (const raw of src.split("\n")) {
    const line = raw.trim();
    if (line === "") continue;
    if (!line.startsWith("//")) break;
    const m = /^\/\/\s*META:\s*global=(.*)$/.exec(line);
    if (m) return m[1].trim();
  }
  return "window,dedicatedworker";
}

// Which scopes to run for a file. A `global=worker`-only file runs worker-scope;
// otherwise window-scope (the default + the cheaper, sufficient pass for the
// messaging/clone battery, which is global-agnostic). The status entry can pin
// `scopes` explicitly to force both.
function scopesFor(rel, entry) {
  if (entry.scopes) return entry.scopes;
  const g = metaGlobals(rel);
  const tokens = g.split(",").map((s) => s.trim().toLowerCase());
  const hasWindow = tokens.includes("window");
  return hasWindow ? ["window"] : ["worker"];
}

function runOne(rel, scope) {
  return new Promise((res) => {
    const child = spawn(NUB, [DRIVER, WPT_ROOT, rel, scope], {
      stdio: ["ignore", "pipe", "pipe"],
      // Pin nub's Node to THIS harness's Node (the matrix leg's setup-node Node).
      // NODE_EXECUTABLE bypasses pin-file/engines resolution, so nub can't upgrade
      // past the floor via the repo-root engines.node >=22.15.0 — the floor legs
      // run the real floor Node. (See the NODE_MAJOR comment above.)
      env: { ...process.env, NODE_EXECUTABLE: process.execPath },
    });
    let out = "";
    let err = "";
    let killed = false;
    const timer = setTimeout(() => {
      killed = true;
      child.kill("SIGKILL");
    }, PER_FILE_TIMEOUT_MS);
    child.stdout.on("data", (d) => (out += d));
    child.stderr.on("data", (d) => (err += d));
    child.on("close", () => {
      clearTimeout(timer);
      if (killed) {
        return res({ rel, scope, fileError: `timeout after ${PER_FILE_TIMEOUT_MS}ms`, results: [] });
      }
      const marker = out.lastIndexOf("__WPT_RESULT__");
      if (marker < 0) {
        return res({
          rel,
          scope,
          fileError: "no result emitted" + (err ? ` (stderr: ${err.trim().slice(0, 300)})` : ""),
          results: [],
        });
      }
      try {
        const json = out.slice(marker + "__WPT_RESULT__".length).split("\n")[0];
        res(JSON.parse(json));
      } catch (e) {
        res({ rel, scope, fileError: "result parse: " + e.message, results: [] });
      }
    });
  });
}

// WPT subtest status codes: 0 PASS, 1 FAIL, 2 TIMEOUT, 3 NOTRUN, 4 PRECONDITION_FAILED.
const PASS = 0;

(async () => {
  let unexpectedFail = 0;
  let unexpectedPass = 0;
  let pass = 0;
  let expectedFail = 0;
  let skipped = 0;
  let fileErrors = 0;
  const report = [];

  for (const rel of files) {
    const entry = status.tests[rel];
    // skip can be a string (unconditional) or {versioned:[{minMajor?,maxMajor?,reason}]}
    // (version-gated: only skip when NODE_MAJOR falls inside a listed band).
    const skipReason = (() => {
      if (typeof entry.skip === "string") return entry.skip;
      if (entry.skip && entry.skip.versioned) {
        for (const v of entry.skip.versioned) {
          const okMin = v.minMajor == null || NODE_MAJOR >= v.minMajor;
          const okMax = v.maxMajor == null || NODE_MAJOR <= v.maxMajor;
          if (okMin && okMax) return v.reason;
        }
      }
      return null;
    })();
    if (skipReason) {
      skipped++;
      report.push(`  skip  ${rel}  (${skipReason})`);
      continue;
    }
    const expectedFails = expectedFailsFor(entry);
    // Track which expected-fail names actually matched a subtest, so a stale/typo'd
    // entry (a name that matches nothing — e.g. an upstream rename) is surfaced
    // rather than silently rotting and quietly holding a divergent subtest to
    // must-pass. Reconciled per file after all its scopes run. NOTE: a versioned
    // expected-fail only contributes names on the versions it targets, so the
    // stale-name check below is naturally scoped to THIS Node — a name that applies
    // only to Node 22 is not flagged stale on Node 24 because it isn't in the set.
    const matchedExpected = new Set();

    for (const scope of scopesFor(rel, entry)) {
      const r = await runOne(rel, scope);
      const label = `${rel}${scope === "worker" ? " [worker]" : ""}`;
      if (r.fileError) {
        fileErrors++;
        unexpectedFail++;
        report.push(`  ERROR ${label}  ${r.fileError}`);
        continue;
      }
      // A non-OK harness status is a WHOLE-FILE failure (async uncaught exception,
      // `done()` with no tests, harness timeout, or our watchdog trip) that NO
      // subtest reflects — counting only subtests would report it as a false green.
      if (r.harnessStatus) {
        unexpectedFail++;
        report.push(`  FAIL  ${label}  harness ${r.harnessStatus.status}: ${r.harnessStatus.message || ""}`);
        if (process.env.WPT_STREAM) console.error(`  FAIL  ${label}  harness ${r.harnessStatus.status}`);
        continue;
      }
      // A file that produced ZERO subtests but is NOT a skip is suspect: its
      // async test likely timed out without ever asserting (a silent non-pass that
      // an "ok (0 subtests)" line would hide). Surface it as an unexpected failure
      // unless the status entry explicitly allows it via `allowEmpty: true`.
      if (r.results.length === 0 && !entry.allowEmpty) {
        unexpectedFail++;
        report.push(`  FAIL  ${label}  produced 0 subtests (timed out without asserting?) — mark {skip} or {allowEmpty:true}`);
        if (process.env.WPT_STREAM) console.error(`  FAIL  ${label}  0 subtests`);
        continue;
      }
      const fails = [];
      const unexpPasses = [];
      for (const sub of r.results) {
        const isExpectedFail = expectedFails.has(sub.name);
        if (isExpectedFail) matchedExpected.add(sub.name);
        if (sub.status === PASS) {
          if (isExpectedFail) {
            unexpPasses.push(sub.name);
            unexpectedPass++;
          } else {
            pass++;
          }
        } else {
          if (isExpectedFail) {
            expectedFail++;
          } else {
            fails.push(sub);
            unexpectedFail++;
          }
        }
      }
      let lines;
      if (fails.length === 0 && unexpPasses.length === 0) {
        lines = [`  ok    ${label}  (${r.results.length} subtests)`];
      } else {
        lines = [`  FAIL  ${label}`];
        for (const f of fails) {
          lines.push(`          ✗ ${f.name} :: ${(f.message || "").slice(0, 200)}`);
        }
        for (const n of unexpPasses) {
          lines.push(`          ! unexpected PASS (now passing, tighten status): ${n}`);
        }
      }
      report.push(...lines);
      // Stream progress so a slow/hung file is visible live (and a CI log shows
      // forward progress rather than going dark until the very end).
      if (process.env.WPT_STREAM) for (const l of lines) console.error(l);
    }

    // Expected-fail-list rot guard: any name declared in `expected[]` that never
    // matched a real subtest is stale (a typo, or an upstream rename) — fail the run
    // so the status file stays honest. Without this, a stale entry silently holds a
    // since-renamed divergent subtest to must-pass with no warning.
    const stale = [...expectedFails].filter((n) => !matchedExpected.has(n));
    if (stale.length) {
      unexpectedFail += stale.length;
      report.push(`  FAIL  ${rel}  stale expected-fail names (matched no subtest): ${stale.join(" | ")}`);
      if (process.env.WPT_STREAM) console.error(`  FAIL  ${rel}  ${stale.length} stale expected-fail names`);
    }
  }

  console.log("=== nub WPT Worker conformance ===");
  console.log(`nub: ${NUB}`);
  console.log(`WPT pin: ${readFileSync(join(WPT_ROOT, "WPT_COMMIT"), "utf8").trim()}`);
  console.log("");
  for (const line of report) console.log(line);
  console.log("");
  console.log(
    `subtests: ${pass} pass, ${expectedFail} expected-fail, ${unexpectedFail} UNEXPECTED-fail, ` +
      `${unexpectedPass} unexpected-pass | files: ${skipped} skipped, ${fileErrors} file-errors`
  );

  // A run is GREEN iff no unexpected failures (regressions). Unexpected passes are
  // surfaced for follow-up but do not fail the gate (the divergence got better).
  if (unexpectedFail > 0) {
    console.log("RESULT: FAIL (unexpected failures — see ✗ lines above)");
    process.exit(1);
  }
  console.log("RESULT: PASS");
  process.exit(0);
})();
