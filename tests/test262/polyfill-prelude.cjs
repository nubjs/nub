// test262-harness runs tests in a VM realm. Evaluate the actual CommonJS source
// there (a normal require would install into the host realm instead).
(function install() {
  const nodeGlobal = require("node:vm").runInThisContext("globalThis");
  for (const name of ["process", "Buffer", "URL", "URLSearchParams", "TextDecoder", "TextEncoder", "queueMicrotask", "setTimeout", "clearTimeout"]) {
    if (!(name in globalThis)) globalThis[name] = nodeGlobal[name];
  }
  const file = process.env.NUB_TEST262_POLYFILLS;
  if (!file) throw new Error("NUB_TEST262_POLYFILLS must name the polyfill source");
  const ip = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
  const aip = Object.getPrototypeOf(Object.getPrototypeOf(Object.getPrototypeOf((async function* () {})())));
  for (const [holder, keys] of [
    [Math, ["sumPrecise"]], [Intl.Locale.prototype, ["variants"]],
    [Promise, ["try"]], [ip, ["join", "includes", "take", "drop"]],
    [aip, [Symbol.asyncDispose]],
  ]) for (const key of keys) delete holder[key];
  if (globalThis.Iterator) for (const key of ["zip", "zipKeyed"]) delete Iterator[key];
  const source = require("node:fs").readFileSync(file, "utf8");
  const runtimeRequire = require("node:module").createRequire(file);
  const primordialFile = runtimeRequire.resolve("./primordials.cjs");
  const primordialModule = { exports: {} };
  Function("module", require("node:fs").readFileSync(primordialFile, "utf8"))(primordialModule);
  const realmRequire = (id) => id === "./primordials.cjs" ? primordialModule.exports : runtimeRequire(id);
  const module = { exports: {} };
  Function("require", "module", "__filename", source)(realmRequire, module, file);
  module.exports.installSyncPolyfills();
  const createRealm = $262.createRealm;
  $262.createRealm = function (options) {
    const realm = createRealm.call(this, options);
    const completion = realm.evalScript(`(${install.toString()})()`);
    if (completion.type === "throw") throw completion.value;
    return realm;
  };
})();
