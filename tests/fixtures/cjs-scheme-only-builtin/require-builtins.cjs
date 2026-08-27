// Every builtin that can ONLY be required WITH the `node:` scheme, required from
// CommonJS. Self-enumerating so the fixture is meaningful on every Node in the CI
// matrix: `node:sqlite` (22.5+), `node:sea` (21.7+) and `node:test/reporters`
// (19.9+) simply drop out of the list on a Node that predates them.
const { isBuiltin } = require("node:module");
const assert = require("node:assert");

const CANDIDATES = ["node:test", "node:test/reporters", "node:sqlite", "node:sea"];
const schemeOnly = CANDIDATES.filter((s) => isBuiltin(s) && !isBuiltin(s.slice("node:".length)));

const failures = [];
for (const spec of schemeOnly) {
  try {
    assert.notStrictEqual(require(spec), undefined, `${spec} loaded as undefined`);
  } catch (err) {
    failures.push(`${spec}: ${err.code || err.name}: ${err.message}`);
  }
}

// Neighbours that must be untouched: a REGULAR builtin (never affected, since its
// bare id already round-tripped to `node:fs`) and a project file whose basename
// collides with a scheme-only builtin id.
assert.strictEqual(typeof require("node:fs").readFileSync, "function");
assert.strictEqual(require("./local-test.js").tag, "local");

if (failures.length > 0) {
  console.error(`FAILED ${failures.join(" | ")}`);
  process.exit(1);
}
console.log(`OK ${schemeOnly.length} ${schemeOnly.join(",")}`);
