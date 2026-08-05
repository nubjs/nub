// Every API the runtime docs promise, probed by presence AND by one real call —
// a global that exists but throws is the failure a `typeof` check would miss.
const t = (name, fn) => { try { return fn() ? "Y" : "n"; } catch { return "!"; } };
const r = {
  Temporal: t("", () => Temporal.Now.instant().epochMilliseconds > 0),
  URLPattern: t("", () => new URLPattern({ pathname: "/a/:b" }).test("https://x/a/1")),
  RegExpEscape: t("", () => new RegExp(RegExp.escape("a.b")).test("a.b")),
  ErrorIsError: t("", () => Error.isError(new Error("x"))),
  PromiseTry: t("", () => Promise.try(() => 1) instanceof Promise),
  PromiseWithResolvers: t("", () => typeof Promise.withResolvers().resolve === "function"),
  ObjectGroupBy: t("", () => Object.groupBy([1], x => x)["1"] !== undefined),
  MapGroupBy: t("", () => Map.groupBy([1], x => x).size === 1),
  ArrayToSorted: t("", () => [2,1].toSorted()[0] === 1),
  StringIsWellFormed: t("", () => "a".isWellFormed()),
  ArrayBufferTransfer: t("", () => new ArrayBuffer(8).transfer().byteLength === 8),
  URLParse: t("", () => URL.parse("https://x/") !== null),
  AtomicsPause: t("", () => (Atomics.pause(), true)),
  SetUnion: t("", () => new Set([1]).union(new Set([2])).size === 2),
  ArrayFromAsync: t("", () => Array.fromAsync([1]) instanceof Promise),
  IteratorHelpers: t("", () => [1,2].values().map(x => x).toArray().length === 2),
  IteratorConcat: t("", () => Iterator.concat([1].values()).toArray().length === 1),
  MapGetOrInsert: t("", () => new Map().getOrInsert("k", 1) === 1),
  IteratorZip: t("", () => Iterator.zip([[1].values(), [2].values()]).toArray().length === 1),
  IteratorChunks: t("", () => [1,2].values().chunks(2).toArray().length === 1),
  MathSumPrecise: t("", () => Math.sumPrecise([1,2]) === 3),
  SymbolMetadata: t("", () => typeof Symbol.metadata === "symbol"),
  PromiseAllKeyed: t("", () => Promise.allKeyed({}) instanceof Promise),
  Float16Array: t("", () => new Float16Array(1).length === 1),
  navigator: t("", () => typeof navigator.hardwareConcurrency === "number"),
  navigatorLocks: t("", () => typeof navigator.locks.request === "function"),
  reportError: t("", () => typeof reportError === "function"),
};
console.log("ok:" + Object.entries(r).map(([k, v]) => k + "=" + v).join(","));
