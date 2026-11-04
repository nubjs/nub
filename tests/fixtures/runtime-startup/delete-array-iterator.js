// A script may remove the array iterator before it imports (Node's own suite
// does, in test-require-delete-array-iterator.js); the load hook still runs.
"use strict";
delete Array.prototype[Symbol.iterator];
import("./esm-ok.mjs").then((m) => {
  process.stdout.write(JSON.stringify({ ok: m.ok }) + "\n");
});
