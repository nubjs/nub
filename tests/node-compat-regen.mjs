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

// Divergences the Rust gate sees that the cross-runtime run does not, because
// the gate runs inside a full Node checkout (tests/node-suite) rather than a
// bare test/ tree: Node's repo tsconfig.json maps bare names such as
// `internal/*` onto ./lib through `paths`, which nub honours for require(), so
// the "cannot find module 'internal/...'" these tests assert never happens
// there. The fd probe depends on how the harness wires stderr.
const GATE_ENVIRONMENT = {
  "parallel/test-internal-modules.js": "layout-artifact: the Node checkout's tsconfig.json paths map internal/* onto lib/, which nub resolves; passes on a bare test/ tree",
  "es-module/test-loaders-hidden-from-users.js": "layout-artifact: the Node checkout's tsconfig.json paths map internal/* onto lib/, which nub resolves; passes on a bare test/ tree",
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
  entries[file] = { ignore: true, reason: categorize(file, r.nub) };
}

const nodeV = results.meta.binaries.node.version;
const nubV = results.meta.binaries.nub.version;
const header = [
  "// Nub Node.js compatibility test configuration.",
  "// Every test in parallel/, sequential/, es-module/, module-hooks/, async-hooks/",
  "// and test-runner/ that real Node passes on the corpus it was generated from.",
  "// An empty entry passes under nub too; \"ignore\": true marks a divergence (nub",
  "// fails, node passes) with its reason. Regenerated by tests/node-compat-regen.mjs",
  `// from tests/cross-runtime/results.json generated ${results.meta.generatedAt} (corpus Node v${results.meta.corpusNodeVersion}, node ${nodeV}, nub ${nubV}).`,
  "// Do not hand-edit entries; edit a reason, or re-run the generator.",
];
const body = Object.keys(entries).sort().map((k) => {
  const v = entries[k];
  return v.ignore ? `  ${JSON.stringify(k)}: { "ignore": true, "reason": ${JSON.stringify(v.reason)} },` : `  ${JSON.stringify(k)}: {},`;
});
body[body.length - 1] = body[body.length - 1].replace(/,$/, "");
fs.writeFileSync(CONFIG, `${header.join("\n")}\n{\n${body.join("\n")}\n}\n`);
console.log(`${CONFIG}: ${pass + diverge} entries (${pass} pass, ${diverge} ignore — ${carried} reasons carried forward, ${diverge - carried} untriaged)`);
