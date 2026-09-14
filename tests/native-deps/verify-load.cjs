// Verifies that the native deps the fixture approves actually load.
// Runs after `nub install` in the native-deps fixture directory, once both
// approved scripts have run, so a module that does not load is a failure.

// esbuild — its postinstall checks for, or downloads, the platform binary.
const esbuild = require("esbuild");
if (typeof esbuild.buildSync !== "function") {
  console.error("FAIL: esbuild.buildSync is not a function");
  process.exit(1);
}
console.log("ok: esbuild loaded, version", esbuild.version);

// better-sqlite3 — its install script fetched a prebuilt N-API addon or
// compiled one with node-gyp.
const Database = require("better-sqlite3");
const db = new Database(":memory:");
const row = db.prepare("SELECT 1 + 1 AS result").get();
if (row.result !== 2) {
  console.error("FAIL: better-sqlite3 query returned wrong result:", row.result);
  process.exit(1);
}
db.close();
console.log("ok: better-sqlite3 loaded, in-memory query passed");

console.log("NATIVE-DEPS-OK");
