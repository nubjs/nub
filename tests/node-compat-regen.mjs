#!/usr/bin/env node
// Regenerate tests/node-compat-config.jsonc from a tests/cross-runtime/run.mjs
// results file.
//
// The config lists every test in the JS-executable directories below that real
// Node passes on the corpus version; an entry is `{}` when nub passes it too and
// `{ "ignore": true, "reason": ... }` when nub does not. Reasons already in the
// config are carried forward for entries that still diverge, so the curated
// categorisation survives a regeneration; a new divergence gets the tail of its
// output as a provisional reason, for triage.
//
//   node tests/node-compat-regen.mjs [--results tests/cross-runtime/results.json]
//                                    [--config tests/node-compat-config.jsonc]
//
// Directories: the ones the Rust gate (crates/nub-cli/tests/node_compat.rs) can
// run as `nub <file>` with exit 0 == pass. abort/ expects a crash, message/ and
// pseudo-tty/ need Node's python runner, internet/ needs the network.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const argValue = (flag, dflt) => {
  const i = process.argv.indexOf(flag);
  return i === -1 ? dflt : process.argv[i + 1];
};
const RESULTS = path.resolve(argValue("--results", path.join(HERE, "cross-runtime/results.json")));
const CONFIG = path.resolve(argValue("--config", path.join(HERE, "node-compat-config.jsonc")));
const DIRS = ["parallel", "sequential", "es-module", "module-hooks", "async-hooks", "test-runner"];

const results = JSON.parse(fs.readFileSync(RESULTS, "utf8"));
if (!results.results || !results.meta?.binaries?.node || !results.meta?.binaries?.nub) {
  throw new Error(`${RESULTS} must come from a run that included both node and nub`);
}

const previous = (() => {
  try {
    const src = fs.readFileSync(CONFIG, "utf8").replace(/^\s*\/\/.*$/gm, "");
    return JSON.parse(src);
  } catch { return {}; }
})();

// Known divergence classes, matched on the test path or nub's failure output.
// First match wins; anything else is "untriaged" with the most informative
// line of output, for a human to classify.
const CLASSES = [
  [(f, t) => /--permission requires --allow-addons/.test(t), "permission-model: nub refuses --permission without --allow-addons because its transpiler is a native addon (use --node)"],
  [(f) => /test-internal-modules\.js$|test-loaders-hidden-from-users\.js$/.test(f), "layout-artifact: the Node checkout's tsconfig.json maps internal/* onto lib/ through paths, which nub resolves for require(), so the module-not-found the test asserts never happens"],
  [(f) => /^parallel\/test-process-versions\.js$/.test(f), "process-versions: nub adds process.versions.nub, so the fixed key list the test deep-equals no longer matches"],
  [(f) => /^parallel\/test-runner-coverage-default-exclusion\.mjs$/.test(f), "coverage-exclude: a grandchild spawned through process.execPath inherits nub's runtime exclude from NODE_OPTIONS, which turns Node's default test-file exclusion off there; nub restates the default only on its own argv so a grandchild's own exclude keeps working, so the two no-exclude cases see test files in their report"],
  [(f) => /^parallel\/test-repl\.js$/.test(f), "layout-artifact: the Node checkout's tsconfig.json maps internal/* onto lib/ through paths, which nub resolves for require(), so the module-not-found the test asserts never happens"],
  [(f) => /^parallel\/test-(os-checked-function|tls-clientcertengine-unsupported|tls-keyengine-unsupported)\.js$/.test(f), "feature-enabled: nub injects --experimental-eventsource, which pulls node:os and internal/tls/secure-context into the bootstrap graph, so the test's monkey-patch of the internal binding lands after the module already captured the original"],
  [(f) => /^parallel\/test-permission-config-file\.mjs$/.test(f), "permission-model: the --permission child the test spawns inherits nub's NODE_OPTIONS --require=preload.cjs and is denied fs-read before user code runs (use --node)"],
  [(f) => /^es-module\/test-esm-cjs-named-error\.mjs$/.test(f), "source-maps: Node gates its CommonJS named-export hint on source maps being OFF (module_job.js), and nub injects --enable-source-maps"],
  [(f) => /^parallel\/test-source-map-enable\.js$/.test(f), "source-maps: the test asserts a stack is NOT source-mapped without the flag, but nub's NODE_OPTIONS carries --enable-source-maps into the spawned child"],
  [(f) => /^parallel\/test-runner-coverage(-source-map|-thresholds)?\.(js|mjs)$/.test(f), "source-maps: reproduced by node --enable-source-maps alone on this corpus, which nub injects; Node 26.0-26.7 changes test-runner coverage output and turns a no-message assert() failure into ERR_INVALID_ARG_TYPE under it"],
  [(f) => /^parallel\/test-runner-flag-propagation\.js$/.test(f), "preload: nub's NODE_OPTIONS --require and --test-coverage-exclude are array-valued options, so the child reads them back alongside the flag under test"],
  [(f) => /^es-module\/test-require-node-modules-warning\.js$/.test(f), "preload: nub's own preload.cjs require()s an ES module from the runtime dir, which --trace-require-module=no-node-modules reports on stderr"],
  [(f) => /^es-module\/test-require-module-tla-error-snapshot\.mjs$/.test(f), "preload: nub's Module._load wrapper adds a frame to the stderr the snapshot pins"],
  [(f) => /^parallel\/test-util-inspect\.js$/.test(f), "preload: util.inspect dims only node:internal and node_modules stack frames, and nub's preload frame is neither, so the colour assertion fails"],
  [(f) => /^es-module\/test-typescript\.mjs$/.test(f), "transpiler: nub's oxc transpiler supersedes Node's type stripping, so the enum, decorator, namespace, generics, extensionless-import and invalid-syntax errors the test asserts never occur"],
  [(f) => /^parallel\/test-debugger-probe-typescript\.js$/.test(f), "transpiler: oxc emits reflowed output where Node's stripper preserves positions in place, so the inspector reports the post-transpile line and column"],
  [(f) => /^es-module\/test-esm-module-not-found-commonjs-hint\.mjs$/.test(f), "resolve-extensions: nub's additive resolver extension-probes a bare package subpath, so the import resolves and Node's did-you-mean hint never fires"],
  [(f) => /^report\//.test(f), "process.report: nub adds process.versions.nub after the native report snapshot, so componentVersions no longer equals process.versions"],
  [(f) => /^module-hooks\//.test(f) || /loader-hooks|registerHooks/.test(f), "module-hooks: nub's preload registers its own hooks first, so tests asserting on the hook chain see extra hooks"],
  [(f) => /compile-cache/.test(f), "compile-cache: nub's oxc transpile cache supersedes Node's compile cache for the fixture"],
  [(f) => /test-node-output-|^test-runner\/test-output-/.test(f), "output-snapshot: nub's preload adds frames and paths to stack traces, so the stderr snapshot differs"],
  [(f, t) => /test-assert|test-runner-assert/.test(f) && /actual: \[TypeError\]/.test(t), "source-maps: Node 26.0-26.7 throws TypeError from a no-message assert() under --enable-source-maps (nodejs/node#63169), which nub injects; gated by nub PR #784"],
  [(f) => /import-text|import-attributes/.test(f), "feature-enabled: nub injects --experimental-import-text, so the unsupported-attribute error the test expects does not occur"],
  [(f, t) => /ExperimentalWarning|expectWarning|warning/i.test(t), "warnings: nub suppresses ExperimentalWarning and injects flags that emit or reorder warnings"],
  [(f, t) => /preload-common\.cjs|preload\.cjs|runtime\/preload/.test(t), "preload: the failure trace runs through nub's preload (augmentation-layer divergence)"],
];
function categorize(file, r) {
  const tail = r?.tail || "";
  for (const [test, reason] of CLASSES) if (test(file, tail)) return reason;
  if (r?.timeout) return "untriaged: timeout";
  const lines = tail.split("\n").map((l) => l.trim()).filter((l) => l && !/^Node\.js v/.test(l) && !/^[}\]]/.test(l));
  const best = lines.find((l) => /Error|nub:|Expected|expected/.test(l)) || lines[lines.length - 1] || `exit ${r?.exit}`;
  return `untriaged: ${best.slice(0, 160)}`;
}
// Reasons must survive the naive `//` strippers that read this file (the Rust
// gate truncates a line at its first `//`; the shell runner does the same), so
// no reason may contain `//` or a double quote. Any run of slashes collapses
// to one so the result is a fixed point (`file:///x` -> `file:/x`).
const sanitize = (reason) => reason.replace(/\/{2,}/g, "/").replace(/"/g, "'");

// Divergences the Rust gate may see that the cross-runtime run does not: one
// depends on how the harness wires stderr, one on how deep the checkout sits.
const GATE_ENVIRONMENT = {
  "parallel/test-fs-cp-async-socket.mjs": "harness-path: the test binds a Unix socket under the checkout's test/.tmp.<id>/, and a checkout path deep enough (a worktree under ~/.cache) pushes it past sun_path's 104 bytes, so it dies with EINVAL before its own assertion; passes from a short checkout",
  "parallel/test-listen-fd-ebadf.js": "harness-stdio: the test probes fd 2, whose type depends on how the gate wires stderr; passes standalone",
};

const entries = {};
let pass = 0, diverge = 0, carried = 0;
for (const [file, r] of Object.entries(results.results)) {
  if (!DIRS.includes(file.split("/")[0])) continue;
  if (!r.node?.pass) continue; // node itself fails it on this corpus/Node: not a nub question
  if (GATE_ENVIRONMENT[file]) { entries[file] = { ignore: true, reason: GATE_ENVIRONMENT[file] }; diverge++; carried++; continue; }
  if (r.nub?.pass) { entries[file] = {}; pass++; continue; }
  diverge++;
  const prev = previous[file];
  if (prev?.ignore && prev.reason && !prev.reason.startsWith("untriaged:")) { entries[file] = { ignore: true, reason: prev.reason }; carried++; continue; }
  const reason = categorize(file, r.nub);
  // A divergence the classifier could not name is recorded as `untriaged`,
  // not `ignore`: the gate still skips it, but counts and reports it apart
  // from the deliberately classified ones, so it cannot hide among them.
  entries[file] = reason.startsWith("untriaged:") ? { untriaged: true, reason } : { ignore: true, reason };
}

const nodeV = results.meta.binaries.node.version;
const nubV = results.meta.binaries.nub.version;
const header = [
  "// Nub Node.js compatibility test configuration.",
  "// Every test in parallel/, sequential/, es-module/, module-hooks/, async-hooks/",
  "// and test-runner/ that real Node passes on the corpus it was generated from.",
  "// An empty entry passes under nub too; \"ignore\": true marks a classified divergence",
  "// (nub fails, node passes) with its reason; \"untriaged\": true marks one the",
  "// generator could not classify — still skipped by the gate, but reported apart.",
  "// Regenerated by tests/node-compat-regen.mjs",
  `// from tests/cross-runtime/results.json generated ${results.meta.generatedAt} (corpus Node v${results.meta.corpusNodeVersion}, node ${nodeV}, nub ${nubV}).`,
  "// Do not hand-edit entries; edit a reason, or re-run the generator.",
];
const body = Object.keys(entries).sort().map((k) => {
  const v = entries[k];
  if (v.ignore) return `  ${JSON.stringify(k)}: { "ignore": true, "reason": ${JSON.stringify(sanitize(v.reason))} },`;
  if (v.untriaged) return `  ${JSON.stringify(k)}: { "untriaged": true, "reason": ${JSON.stringify(sanitize(v.reason))} },`;
  return `  ${JSON.stringify(k)}: {},`;
});
body[body.length - 1] = body[body.length - 1].replace(/,$/, "");
fs.writeFileSync(CONFIG, `${header.join("\n")}\n{\n${body.join("\n")}\n}\n`);
const untriaged = Object.values(entries).filter((v) => v.untriaged).length;
console.log(`${CONFIG}: ${pass + diverge} entries (${pass} pass, ${diverge - untriaged} ignore with a classified reason — ${carried} carried forward — and ${untriaged} untriaged)`);
