// Nub reaches these by injecting an --experimental-* flag, not by polyfilling,
// so the artifact proves the LAUNCHER computed the same flag set for this Node.
const t = async (fn) => { try { return (await fn()) ? "Y" : "n"; } catch { return "!"; } };
const r = {
  vmModule: await t(async () => new (await import("node:vm")).SourceTextModule("export const a=1")),
  WebSocket: await t(() => typeof WebSocket === "function"),
  EventSource: await t(() => typeof EventSource === "function"),
  sqlite: await t(async () => new (await import("node:sqlite")).DatabaseSync(":memory:")),
  streamIter: await t(async () => (await import("node:stream/iter")) !== undefined),
  ffi: await t(async () => (await import("node:ffi")) !== undefined),
  vfs: await t(async () => (await import("node:vfs")) !== undefined),
};
console.log("ok:" + Object.entries(r).map(([k, v]) => k + "=" + v).join(","));
