// Differential web-locks check: every assertion holds on BOTH nub's polyfill
// (Node < 24.5) and native Web Locks (24.5+), so it passes on every CI Node leg —
// a regression on either path turns it red. Covers the spec behaviors that the WPT
// suite exercises and that the hand-rolled polyfill previously got wrong: steal,
// option/name validation, reader/writer fairness, AbortSignal, and core mutual
// exclusion. Hang-proof: each scenario is time-bounded and the process exits.
const checks = [];
const withTimeout = (p, ms, label) =>
  Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error("timeout/deadlock " + label)), ms))]);
const check = async (name, fn) => {
  try { await withTimeout(fn(), 1500, name); checks.push([name, true]); }
  catch (e) { checks.push([name, false, String((e && e.message) || e)]); }
};
let n = 0;
const uniq = () => `r-${++n}`;
const locks = globalThis.navigator.locks;

await check("core mutual exclusion serializes exclusive holders", async () => {
  const r = uniq();
  const order = [];
  let release;
  const first = locks.request(r, () => new Promise((res) => { release = res; order.push("a"); }));
  const second = locks.request(r, () => { order.push("b"); });
  await new Promise((res) => setTimeout(res, 20));
  if (order.join(",") !== "a") throw new Error("second ran while first held: " + order);
  release();
  await second;
  if (order.join(",") !== "a,b") throw new Error("order: " + order);
});

await check("shared holders coexist; exclusive waits", async () => {
  const r = uniq();
  let unblock; const blocked = new Promise((res) => { unblock = res; });
  const g = [];
  locks.request(r, { mode: "shared" }, async () => { g.push("s1"); await blocked; });
  locks.request(r, { mode: "shared" }, async () => { g.push("s2"); await blocked; });
  const ex = locks.request(r, async () => { g.push("ex"); });
  await new Promise((res) => setTimeout(res, 20));
  if (g.join(",") !== "s1,s2") throw new Error("shared not co-granted: " + g);
  unblock(); await ex;
  if (g[2] !== "ex") throw new Error("exclusive not after shared: " + g);
});

await check("reader/writer fairness: new shared queues behind pending exclusive", async () => {
  const r = uniq();
  let relShared; const sharedHold = new Promise((res) => { relShared = res; });
  let relEx; const exHold = new Promise((res) => { relEx = res; });
  const firstShared = Promise.all([0, 0, 0].map(() => locks.request(r, { mode: "shared" }, () => sharedHold)));
  locks.request(r, () => exHold);                       // exclusive queues behind the held shared
  for (let i = 0; i < 3; i++) locks.request(r, { mode: "shared" }, () => new Promise(() => {})); // must NOT barge ahead
  let q = await locks.query();
  if (q.held.filter((l) => l.name === r).length !== 3) throw new Error("held != 3: " + q.held.length);
  relShared(); await firstShared;
  q = await locks.query();
  const heldHere = q.held.filter((l) => l.name === r);
  if (heldHere.length !== 1 || heldHere[0].mode !== "exclusive") throw new Error("exclusive should hold next, got " + JSON.stringify(heldHere));
  relEx();
});

await check("ifAvailable yields null when not grantable", async () => {
  const r = uniq();
  await locks.request(r, async () => {
    const got = await locks.request(r, { ifAvailable: true }, (l) => { if (l !== null) throw new Error("expected null"); return 7; });
    if (got !== 7) throw new Error("result " + got);
  });
});

await check("steal grants and breaks the current holder with AbortError", async () => {
  const r = uniq();
  let brokenName = null;
  locks.request(r, () => new Promise(() => {})).catch((e) => { brokenName = e && e.name; });
  let stolen = false;
  await locks.request(r, { steal: true }, () => { stolen = true; });
  await new Promise((res) => setTimeout(res, 30));
  if (!stolen) throw new Error("steal callback never ran");
  if (brokenName !== "AbortError") throw new Error("broken holder name = " + brokenName);
});

await check("signal: non-AbortSignal throws TypeError", async () => {
  let err = null;
  try { await locks.request(uniq(), { signal: {} }, () => {}); } catch (e) { err = e; }
  if (!(err instanceof TypeError)) throw new Error("got " + (err && err.constructor.name));
});

await check("signal: already-aborted rejects with the reason", async () => {
  const ctrl = new AbortController();
  const reason = new Error("nope");
  ctrl.abort(reason);
  let err = null;
  try { await locks.request(uniq(), { signal: ctrl.signal }, () => { throw new Error("callback ran"); }); } catch (e) { err = e; }
  if (err !== reason) throw new Error("reason mismatch: " + err);
});

await check("name starting with '-' rejects NotSupportedError", async () => {
  let name = null;
  try { await locks.request("-bad", () => {}); } catch (e) { name = e && e.name; }
  if (name !== "NotSupportedError") throw new Error("got " + name);
});

await check("invalid option combo steal+ifAvailable rejects NotSupportedError", async () => {
  let name = null;
  try { await locks.request(uniq(), { steal: true, ifAvailable: true }, () => {}); } catch (e) { name = e && e.name; }
  if (name !== "NotSupportedError") throw new Error("got " + name);
});

const ok = checks.every((c) => c[1]);
for (const c of checks) if (!c[1]) console.log("weblocks:FAIL:" + c[0] + " :: " + c[2]);
console.log(ok ? "weblocks:ALL-OK" : "weblocks:FAILED");
process.exit(ok ? 0 : 1);
