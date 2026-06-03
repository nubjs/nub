// Verify Nub's Temporal lazy-global polyfill is installed.
// Per runtime/polyfills.mjs, Temporal is provided as a lazy global on all
// supported Node versions (it is in no Node release as of v24.x).
const T = (globalThis as unknown as { Temporal?: { Now: { plainDateISO(): { toString(): string } } } }).Temporal;
if (!T) {
  console.error("Temporal global missing");
  process.exit(2);
}
const today = T.Now.plainDateISO().toString();
// Print only that we got a YYYY-MM-DD-shaped string, not the literal date
// (so the test is deterministic).
const shape = /^\d{4}-\d{2}-\d{2}$/.test(today) ? "YYYY-MM-DD" : "unexpected";
console.log(`temporal: ${shape}`);
