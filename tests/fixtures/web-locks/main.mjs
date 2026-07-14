// Cross-version Web Locks check. Shared behavior is asserted on both nub's polyfill
// (Node < 24.5) and native Web Locks (24.5+). Where current native Node diverges
// from Web IDL dictionary conversion, the fixture requires spec behavior from the
// polyfill and records the native deviation explicitly. Hang-proof: each scenario
// is time-bounded and the process exits.
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
const [nodeMajor, nodeMinor] = process.versions.node.split(".").map(Number);
const nativeLocksTier = nodeMajor > 24 || (nodeMajor === 24 && nodeMinor >= 5);

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

await check("option dictionaries follow the WebIDL object boundary", async () => {
  for (const options of [null, undefined]) {
    await locks.request(uniq(), options, () => {});
  }

  let functionModeReads = 0;
  let grantedMode = null;
  const fn = () => {};
  Object.defineProperty(fn, "mode", { get() { functionModeReads++; return "shared"; } });
  await locks.request(uniq(), fn, (lock) => { grantedMode = lock.mode; });
  if (nativeLocksTier) {
    // Native Node through 26.5 diverges by treating callable objects as empty dictionaries.
    if (functionModeReads !== 0 || grantedMode !== "exclusive") {
      throw new Error(`native callable-options behavior changed: reads=${functionModeReads}, mode=${grantedMode}`);
    }
  } else if (functionModeReads !== 1 || grantedMode !== "shared") {
    throw new Error(`polyfill callable dictionary: reads=${functionModeReads}, mode=${grantedMode}`);
  }

  for (const [label, options] of [
    ["number", 1], ["string", "x"], ["boolean", true],
    ["bigint", 1n], ["symbol", Symbol("x")],
  ]) {
    let err = null;
    let called = false;
    try { await locks.request(uniq(), options, () => { called = true; }); } catch (e) { err = e; }
    if (!(err instanceof TypeError) || called) throw new Error(`${label}: ${err && err.constructor.name}, callback=${called}`);
  }
});

await check("overload arity is resolved before argument conversion", async () => {
  let nameConversions = 0;
  const name = { toString() { nameConversions++; return uniq(); } };
  let err = null;
  try { await locks.request(name); } catch (e) { err = e; }
  if (!(err instanceof TypeError)) throw new Error("one argument: " + err);
  if (nativeLocksTier) {
    // Native Node through 26.5 diverges by converting the name before rejecting.
    if (nameConversions !== 1) throw new Error("native name conversions: " + nameConversions);
  } else if (nameConversions !== 0) {
    throw new Error("polyfill name conversions: " + nameConversions);
  }

  const marker = new Error("explicit undefined kept the three-argument overload");
  let optionReads = 0;
  const options = Object.create(null, {
    ifAvailable: { get() { optionReads++; throw marker; } },
  });
  err = null;
  try { await locks.request(uniq(), options, undefined); } catch (e) { err = e; }
  if (nativeLocksTier) {
    // Native Node through 26.5 selects its two-argument path from the callback value.
    if (!(err instanceof TypeError) || optionReads !== 0) {
      throw new Error(`native explicit-undefined order: ${err}, reads=${optionReads}`);
    }
  } else if (err !== marker || optionReads !== 1) {
    throw new Error(`polyfill explicit-undefined order: ${err}, reads=${optionReads}`);
  }
});

await check("three-argument options convert before the callback", async () => {
  const marker = new Error("options converted first");
  let reads = 0;
  const options = Object.create(null, {
    ifAvailable: { get() { reads++; throw marker; } },
  });
  let err = null;
  try { await locks.request(uniq(), options, null); } catch (e) { err = e; }
  if (nativeLocksTier) {
    // Native Node through 26.5 diverges by validating the callback first.
    if (!(err instanceof TypeError) || reads !== 0) throw new Error(`native order: ${err}, reads=${reads}`);
  } else if (err !== marker || reads !== 1) {
    throw new Error(`polyfill order: ${err}, reads=${reads}`);
  }
});

await check("option dictionary members convert in WebIDL order", async () => {
  const events = [];
  const name = { toString() { events.push("convert:name"); return uniq(); } };
  const values = {
    ifAvailable: false,
    mode: { toString() { events.push("convert:mode"); return "exclusive"; } },
    signal: {},
    steal: false,
  };
  const options = new Proxy(values, {
    get(target, key, receiver) {
      events.push(`get:${String(key)}`);
      return Reflect.get(target, key, receiver);
    },
  });
  let err = null;
  try { await locks.request(name, options, () => { events.push("callback"); }); } catch (e) { err = e; }
  if (!(err instanceof TypeError)) throw new Error("got " + (err && err.constructor.name));
  // Native Node through 26.5 defers AbortSignal validation until every member is read.
  const expected = nativeLocksTier
    ? "convert:name,get:ifAvailable,get:mode,convert:mode,get:signal,get:steal"
    : "convert:name,get:ifAvailable,get:mode,convert:mode,get:signal";
  if (events.join(",") !== expected) throw new Error("order: " + events.join(","));
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
