// Nub transform core — the single source of truth shared by both hook tiers.
//
// runtime/preload.mjs (fast path, Node 22.15+, sync `module.registerHooks`) and
// the compat-tier loader worker (Node 18.19–22.14, async `module.register` →
// runtime/preload-async-hooks.mjs) both import every resolution + transpile
// primitive from here. The tier files own only the parts that genuinely differ:
// hook registration (sync vs async signatures), polyfill preloading, the
// Temporal lazy global, watch-mode IPC, and the compat-tier CJS `require()`
// shim. EVERYTHING about how a file is resolved and transpiled — extension
// probing, the `.js`→`.ts` swap, tsconfig `paths`, module-format detection,
// oxc-transform options (including `target: 'es2022'` `using`-lowering), the
// Stage-3 decorator guard, the on-disk cache, data-format imports, package
// clobbering — lives here, so the two tiers can never drift. (They used to:
// separate copies diverged on probe order, `target` lowering, the decorator
// guard, module-format detection, the Temporal clobber's named exports, and the
// reserved-export filter — every one a real compat bug. This module is the fix.)
//
// Side effects are confined to: loading the N-API data addon, lazily loading
// oxc-parser/oxc-transform, and reading/writing the transpile cache. There is no
// top-level hook registration here — importing this module never augments the
// realm; the tier files do that.

// EVERY dependency of this module is pulled in via CJS `require()` (below), NOT
// via static ESM `import`. This is load-bearing for loader compatibility (R11):
// nub loads transform-core through `require(esm)`, and Node's `require(esm)`
// instantiates the module by walking its STATIC IMPORT graph through whatever ESM
// loader hooks are registered — including the USER's `--loader`/`register()`
// chain. Static `import oxc-transform`/`get-tsconfig`/`./version.mjs`/`node:*`
// here therefore leaked nub's entire internal graph (transform-core, version.mjs,
// oxc-transform, get-tsconfig, their transitive node_modules deps, and the node:
// builtins) THROUGH the user's resolve/load hooks, which observed and corrupted
// it (a user load hook returning `source: 1` for version.mjs, a resolve hook
// appending `?foo` to `oxc-transform`, a strict loader throwing on the bare
// `oxc-transform` specifier — see test-esm-loader-chaining, -example-loader,
// -preserve-symlinks-not-found, test-shadow-realm-custom-loaders). Verified: a CJS
// `require()` of a package/builtin does NOT route through the ESM loader chain, so
// loading the graph this way bypasses the user chain entirely. `process
// .getBuiltinModule` fetches node: builtins synchronously off the loader chain;
// `createRequire(import.meta.url)` resolves the bare deps from nub's distribution.
// This file keeps its `export`s (it stays an ES module), but has ZERO static
// imports, so `require(esm)` finds no dependency graph to route through the user.
const { createRequire } = process.getBuiltinModule("node:module");
const __require = createRequire(import.meta.url);

const module = process.getBuiltinModule("node:module");
const { readFileSync, writeFileSync, mkdirSync, statSync, renameSync, unlinkSync, readdirSync } = process.getBuiltinModule("node:fs");
const { fileURLToPath, pathToFileURL } = process.getBuiltinModule("node:url");
const { join, dirname, resolve: pathResolve, extname: pathExtname } = process.getBuiltinModule("node:path");
// oxc-transform is `type: module`, so a plain `require("oxc-transform")` uses
// `require(esm)` and Node instantiates its ESM entry by walking ITS static
// imports through the registered ESM loader chain — i.e. through the USER's
// `--loader` worker (which nub's main-thread sync hooks do NOT chain into, so the
// resolveSpec short-circuit can't catch it). oxc-transform's entry has exactly one
// ESM-only construct that leaks: `import { createRequire } from 'node:module'`
// (the rest is CJS `require`/`module.exports`-shaped). To keep it off the chain
// entirely we load it AS CommonJS: read the entry source, neutralize its tiny ESM
// header (the `import { createRequire }` line, the `const require =
// createRequire(import.meta.url)` shadow, the `import.meta.url`-based `__dirname`)
// and convert its `export { X }` footer to `module.exports`, then compile it with
// `module._compile` at its real path so its internal `require('./binding.node')`
// and `__dirname` resolve correctly. A CJS-compiled module has no static-import
// graph for Node to route through any loader. Falls back to plain require(esm) if
// the source shape ever changes (then the leak returns, but transpilation works).
function requireOxcAsCjs() {
  const Module = module;
  let entry;
  try {
    entry = __require.resolve("oxc-transform");
  } catch {
    return __require("oxc-transform");
  }
  // Only the .js NAPI-RS wrapper has the mixed ESM-header/CJS-body shape we can
  // safely transform; anything else, defer to the normal loader.
  if (!entry.endsWith(".js")) return __require("oxc-transform");
  try {
    let src = readFileSync(entry, "utf8");
    // Strip the ESM header: `import { createRequire } from 'node:module'`,
    // `const require = createRequire(import.meta.url)`, and any
    // `const __dirname = new URL('.', import.meta.url).pathname`. In a CJS module
    // `require`, `__dirname`, `__filename` are already injected by _compile.
    src = src
      .replace(/^\s*import\s*\{[^}]*\}\s*from\s*['"]node:module['"];?\s*$/m, "")
      .replace(/^\s*const\s+require\s*=\s*createRequire\([^)]*\);?\s*$/m, "")
      .replace(/^\s*const\s+__dirname\s*=\s*new URL\([^)]*\)\.pathname;?\s*$/m, "");
    // Convert the `export { Name }` footer lines to a single CJS exports block.
    const names = [];
    src = src.replace(/^\s*export\s*\{\s*([A-Za-z0-9_$]+)\s*\}\s*;?\s*$/gm, (_m, n) => {
      names.push(n);
      return "";
    });
    // Guard: if any `import`/`export`/`import.meta` survived, we don't understand
    // this version's shape — bail to the safe loader rather than ship broken code.
    if (/^\s*(import|export)\s/m.test(src) || /\bimport\.meta\b/.test(src)) {
      return __require("oxc-transform");
    }
    if (names.length === 0) return __require("oxc-transform");
    src += `\nmodule.exports = { ${names.join(", ")} };\n`;
    const m = new Module(entry, null);
    m.filename = entry;
    m.paths = Module._nodeModulePaths(dirname(entry));
    m._compile(src, entry);
    return m.exports;
  } catch {
    return __require("oxc-transform");
  }
}
const { transformSync } = requireOxcAsCjs();
const { getTsconfig, createPathsMatcher } = __require("get-tsconfig");

// NUB_VERSION is the single source of truth in runtime/version.mjs. We must NOT
// `import` it (that would route version.mjs through the user loader chain — see
// above; a user load hook returning bogus source corrupts it), and we cannot
// `require()` it either (it is an ES module, so `require()` uses require(esm),
// which re-routes version.mjs's own load through the chain). Instead read its
// text directly and extract the literal — `make version` keeps the assignment on
// one line (`export const NUB_VERSION = "x.y.z";`), so a tight regex is stable.
const NUB_VERSION = (() => {
  try {
    const text = readFileSync(fileURLToPath(new URL("./version.mjs", import.meta.url)), "utf8");
    const m = text.match(/NUB_VERSION\s*=\s*["']([^"']+)["']/);
    if (m) return m[1];
  } catch {}
  return "0.0.0";
})();

// `node:crypto` is used ONLY to hash the transpile-cache key, so it loads lazily
// on first transpile rather than at module top level. Importing it eagerly pulls
// in the crypto/tls native tree (~dozens of builtins) on EVERY startup — including
// a plain-JS run that never transpiles anything (R7). The first `.ts` transpile
// pays the one-time require; a no-TS run never touches it. Memoized.
let _createHash = null;
function getCreateHash() {
  return (_createHash ??= __require("node:crypto").createHash);
}

// ── Constants ───────────────────────────────────────────────────────
export const TRANSPILE_EXTS = new Set([".ts", ".tsx", ".mts", ".cts", ".jsx"]);
export const DATA_EXTS = { ".jsonc": "jsonc", ".json5": "json5", ".toml": "toml", ".yaml": "yaml", ".yml": "yaml", ".txt": "txt" };
export const TS_PARENT_EXTS = new Set([".ts", ".tsx", ".mts", ".cts"]);

// Reserved words / literals that cannot be a lexical binding name in a module
// (modules are strict mode). A data file with a top-level key like `package`
// (e.g. a Cargo.toml `[package]` table) must NOT emit `export const package = …`
// — that is a SyntaxError that takes down the whole module, default export
// included. Such keys stay reachable via the default export. Matches bun, which
// deoptimizes invalid-identifier keys rather than failing the whole module.
const RESERVED_EXPORT_NAMES = new Set([
  "break", "case", "catch", "class", "const", "continue", "debugger", "default",
  "delete", "do", "else", "enum", "export", "extends", "false", "finally", "for",
  "function", "if", "import", "in", "instanceof", "new", "null", "return", "super",
  "switch", "this", "throw", "true", "try", "typeof", "var", "void", "while", "with",
  // Strict-mode (modules are always strict) future-reserved + restricted names:
  "implements", "interface", "let", "package", "private", "protected", "public",
  "static", "yield", "await", "eval", "arguments",
]);

// Packages resolved from Nub's distribution, not the user's.
export const VENDORED_PACKAGES = new Set(["@oxc-project/runtime"]);

// Built-in modules provided by Nub (resolved to files in this distribution).
// connect() sockets deferred per design decision — "sockets" specifier not clobbered.
export const BUILTIN_MODULES = new Map();

// Package clobbering: specifiers that resolve to a synthetic module re-exporting
// the native global instead of the userland package.
export const CLOBBER_MAP = new Map([
  // Reading globalThis.Temporal triggers the lazy getter the tier file installs,
  // which loads the polyfill by resolved path — that load is what installs
  // Date.prototype.toTemporalInstant, so Temporal MUST be read first.
  // @js-temporal/polyfill exports { Temporal, Intl, toTemporalInstant }; mirror
  // all three so `import { Temporal, Intl, toTemporalInstant } from ...` binds.
  ["@js-temporal/polyfill", () => `const T = globalThis.Temporal; export default T; export const Temporal = T; export const Intl = globalThis.Intl; export const toTemporalInstant = Date.prototype.toTemporalInstant;`],
  ["urlpattern-polyfill", () => `export const URLPattern = globalThis.URLPattern;`],
  ["abort-controller", () => `export const AbortController = globalThis.AbortController; export const AbortSignal = globalThis.AbortSignal; export default globalThis.AbortController;`],
]);

// Nub's N-API addon for data-format parsing (Rust-native YAML/TOML/JSON5/JSONC).
// Loaded once per module instance (= once per thread: the main thread and the
// loader worker each import this module separately).
let nubNative = null;
for (const rel of ["./addons/nub-native.node", "../runtime/addons/nub-native.node"]) {
  try { nubNative = __require(fileURLToPath(new URL(rel, import.meta.url))); break; } catch {}
}

// ── Watch-mode hooks (injected by the main-thread tier) ─────────────
// `nub watch` needs config files (tsconfig.json, package.json) and `.env*` —
// which are not in any import graph — surfaced to Node's FilesWatcher. The main
// thread (preload.mjs) injects reporters; the loader worker injects nothing
// (watch IPC is main-thread only), so these default to no-ops.
let _reportDep = null;
let _reportEnvDir = null;
export function setWatchHooks({ reportDep, reportEnvDir } = {}) {
  if (reportDep) _reportDep = reportDep;
  if (reportEnvDir) _reportEnvDir = reportEnvDir;
}

// ── tsconfig + package-type caches ──────────────────────────────────
const tsconfigCache = new Map();
export function getTsconfigForDir(dir) {
  if (tsconfigCache.has(dir)) return tsconfigCache.get(dir);
  const result = getTsconfig(dir);
  const matcher = result ? createPathsMatcher(result) : null;
  const entry = { tsconfig: result, matcher };
  tsconfigCache.set(dir, entry);
  if (result?.path) _reportDep?.(result.path);
  return entry;
}

// The NEAREST package.json's `type` decides the format of ambiguous extensions
// (.ts/.tsx/.jsx, like Node's .js). The nearest one wins even when its `type`
// is absent — Node does not skip a typeless package.json to find a typed
// ancestor — so we stop at the first package.json found. Returns "module",
// "commonjs", or undefined.
const packageTypeCache = new Map();
export function getPackageType(dir) {
  if (packageTypeCache.has(dir)) return packageTypeCache.get(dir);
  let type;
  let current = dir;
  for (;;) {
    const pkgPath = join(current, "package.json");
    if (fileExists(pkgPath)) {
      try { type = JSON.parse(readFileSync(pkgPath, "utf8")).type; } catch {}
      // Watch this package.json (a `type`/script edit should restart) and the
      // `.env*` files alongside it (the package root is where they live).
      _reportDep?.(pkgPath);
      _reportEnvDir?.(current);
      break;
    }
    const parent = dirname(current);
    if (parent === current) break;
    current = parent;
  }
  packageTypeCache.set(dir, type);
  return type;
}

// ── Filesystem helpers ──────────────────────────────────────────────
export function extname(url) {
  const path = url.includes("?") ? url.slice(0, url.indexOf("?")) : url;
  const dot = path.lastIndexOf(".");
  return dot === -1 ? "" : path.slice(dot);
}

export function isNodeModules(url) {
  return url.includes("/node_modules/") || url.includes("\\node_modules\\");
}

export function fileExists(filePath) {
  const s = statSync(filePath, { throwIfNoEntry: false });
  return s !== undefined && s.isFile();
}

export function dirExists(filePath) {
  const s = statSync(filePath, { throwIfNoEntry: false });
  return s !== undefined && s.isDirectory();
}

function safeRequireResolve(specifier) {
  try { return __require.resolve(specifier); } catch { return null; }
}

export function barePkg(specifier) {
  return specifier.startsWith("@")
    ? specifier.split("/").slice(0, 2).join("/")
    : specifier.split("/")[0];
}

// ── Resolution ──────────────────────────────────────────────────────
// Read a directory's package.json `main` (its legacy CJS entry point), or null.
// `exports` is deliberately NOT consulted: Node honors `exports` only for
// package-name/self-reference resolution, never for a relative/absolute import
// of a directory path (verified against Node 24 — a relative dir import with
// `exports` but no `main` falls through to index, not the export). So matching
// Node here means `main` only.
function readPackageMain(dir) {
  const pkgPath = join(dir, "package.json");
  if (!fileExists(pkgPath)) return null;
  try {
    const main = JSON.parse(readFileSync(pkgPath, "utf8")).main;
    return typeof main === "string" && main.trim() ? main : null;
  } catch {
    return null;
  }
}

// Try to resolve a file path with extensionless probing + .js→.ts swap.
// `allowDirMain` honors a resolved directory's package.json `main` before its
// index; it is cleared on the recursive main-target probe because Node's
// LOAD_AS_DIRECTORY resolves `main` with file+index probing only and does not
// recurse into the target's own nested `main` (verified against Node 24).
export function tryResolveFile(target, parentExt, allowDirMain = true) {
  // If the target already has an extension and exists, use it.
  const existingExt = pathExtname(target);
  if (existingExt && fileExists(target)) return target;

  // .js → .ts swap (tsc emit convention reversal).
  if (existingExt === ".js") {
    const tsSwap = target.slice(0, -3) + ".ts";
    if (fileExists(tsSwap)) return tsSwap;
    const tsxSwap = target.slice(0, -3) + ".tsx";
    if (fileExists(tsxSwap)) return tsxSwap;
  }
  if (existingExt === ".jsx") {
    const tsxSwap = target.slice(0, -4) + ".tsx";
    if (fileExists(tsxSwap)) return tsxSwap;
  }
  // .mjs → .mts swap (Bun does this).
  if (existingExt === ".mjs") {
    const mtsSwap = target.slice(0, -4) + ".mts";
    if (fileExists(mtsSwap)) return mtsSwap;
  }
  // .cjs → .cts swap — the CommonJS analog of .mjs→.mts. tsc resolves
  // `import "./foo.cjs"` to foo.cts (it strips the .cjs and finds the .cts
  // source — verified via --traceResolution), so a TS file using the emitted
  // extension to reference a .cts source must resolve at runtime. (Bun omits
  // this swap even though it does .mjs→.mts; we match tsc, not that gap.)
  if (existingExt === ".cjs") {
    const ctsSwap = target.slice(0, -4) + ".cts";
    if (fileExists(ctsSwap)) return ctsSwap;
  }

  // Extensionless: probe in parent-ext-aware order.
  if (!existingExt) {
    const probeOrder = getProbeOrder(parentExt);
    for (const ext of probeOrder) {
      if (fileExists(target + ext)) return target + ext;
    }
    // Directory: honor package.json `main` (Node's legacy LOAD_AS_DIRECTORY)
    // before falling back to index probing. The main target is resolved with
    // the same extensionless/TS-swap probing (so a TS package can point `main`
    // at a `.ts`, or `.js`→`.ts` swaps apply), but without re-reading a nested
    // `main` — matching Node. If `main` is absent or unresolvable, index wins
    // (Node falls back to index too, with a DEP0128 warning we needn't emit).
    if (dirExists(target)) {
      if (allowDirMain) {
        const main = readPackageMain(target);
        if (main) {
          const resolved = tryResolveFile(pathResolve(target, main), parentExt, false);
          if (resolved) return resolved;
        }
      }
      for (const ext of probeOrder) {
        const idx = join(target, "index" + ext);
        if (fileExists(idx)) return idx;
      }
    }
  }

  return null;
}

export function getProbeOrder(parentExt) {
  switch (parentExt) {
    case ".tsx": return [".tsx", ".ts", ".jsx", ".js", ".json"];
    // .mts/.cts prefer their own module system first, but STILL fall through to
    // the general TS (`.ts`) and JS extensions: tsc and Node resolve an
    // extensionless `./foo` from a .mts/.cts parent to foo.ts / foo.js too, not
    // only foo.mts / foo.cts. Omitting `.ts` here is what made `require('./config')`
    // — and a tsconfig-paths alias — from a .cts (or .mts) parent miss a `.ts`
    // target (works from .js/.cjs, which use the default order below).
    case ".mts": return [".mts", ".ts", ".mjs", ".js", ".json"];
    case ".cts": return [".cts", ".ts", ".cjs", ".js", ".json"];
    default:     return [".ts", ".tsx", ".js", ".jsx", ".json"];
  }
}

// nub's own runtime directory (this file's dir, as a file: URL prefix). Any
// resolution whose IMPORTER lives here is one of nub's internal requires — the
// preload loading transform-core, transform-core requiring oxc, the Temporal lazy
// getter resolving @js-temporal/polyfill — and must NEVER be routed through nub's
// own clobber/vendored/tsconfig logic: those are user-code conveniences, and
// applying them to nub's internals both breaks them (e.g. the Temporal clobber
// re-exports globalThis.Temporal, which IS the getter → a require of the polyfill
// from the getter would recurse into the clobber) and amplifies the user loader
// chain by re-walking nub's internal graph through user hooks (R11). Short-circuit
// to native resolution for these.
const RUNTIME_DIR_URL = new URL(".", import.meta.url).href;

// nub's internal-graph package roots — the file: URL prefixes of the npm packages
// nub itself loads (oxc-transform, get-tsconfig) and their transitive deps. Any
// resolution whose IMPORTER lives under one of these is part of nub's internal
// graph, NOT user code. `oxc-transform` is `type: module`, so even when nub pulls
// it in via CJS `require()`, Node loads its `index.js` through `require(esm)` and
// walks ITS static `import "node:module"` graph through the registered ESM loader
// chain — including the USER's loader. That re-leaks nub internals one hop down
// (R11): test-esm-example-loader's strict loader throws on `node:module`,
// loader-resolve-shortcircuit appends `?foo` to oxc's specifiers, etc. So a
// nub-internal-graph resolution must FULLY short-circuit (resolve natively, return
// shortCircuit:true) — never delegate to nextResolve — for EVERY specifier,
// including node: builtins and the package's own relative imports. Computed lazily
// (and pinned even on resolve failure) so a missing dep can't wedge startup.
let _nubGraphRoots = null;
function nubGraphRoots() {
  if (_nubGraphRoots) return _nubGraphRoots;
  const roots = [];
  for (const pkg of ["oxc-transform", "get-tsconfig"]) {
    try {
      const entry = __require.resolve(pkg);
      // Package root = the directory two levels up does not work generically;
      // instead key on the package-name segment: everything under
      // `.../node_modules/<pkg>/` is that package. Use the entry's dir-with-pkg.
      const idx = entry.lastIndexOf(`${sep()}node_modules${sep()}`);
      if (idx !== -1) {
        // Keep through the package-name segment (handles scoped names too).
        const afterNM = entry.slice(idx + (`${sep()}node_modules${sep()}`).length);
        const firstSeg = afterNM.startsWith("@")
          ? afterNM.split(sep()).slice(0, 2).join(sep())
          : afterNM.split(sep())[0];
        const pkgRoot = entry.slice(0, idx) + `${sep()}node_modules${sep()}` + firstSeg + sep();
        roots.push(pathToFileURL(pkgRoot).href);
      }
    } catch {}
  }
  return (_nubGraphRoots = roots);
}
function sep() {
  return process.platform === "win32" ? "\\" : "/";
}

// Is this importer part of nub's own internal module graph (runtime dir or a nub
// dependency package)? Such imports must bypass the user ESM loader chain entirely.
function isNubInternalParent(parentURL) {
  if (!parentURL) return false;
  const p = String(parentURL);
  if (p.startsWith(RUNTIME_DIR_URL)) return true;
  for (const root of nubGraphRoots()) {
    if (p.startsWith(root)) return true;
  }
  return false;
}

// Resolve a specifier the way both hook tiers do. Returns `{ url, shortCircuit }`
// to short-circuit Node's resolver, or `null` to fall through to `nextResolve`.
// `parentURL` is the importer (a file: URL string), or "" for the entry.
export function resolveSpec(specifier, parentURL) {
  // nub's own internal graph (importer inside nub's runtime dir OR a nub
  // dependency package): resolve natively and SHORT-CIRCUIT so nextResolve (the
  // user's loader chain) never observes nub's internals. This MUST run before the
  // node:/data:/builtin early-returns below, because those `return null` =
  // DELEGATE to the user loader — and a nub-internal `import "node:module"` (e.g.
  // from oxc-transform's ESM entry) delegated to a strict user loader is exactly
  // the R11 leak. See isNubInternalParent / nubGraphRoots.
  if (isNubInternalParent(parentURL)) {
    if (specifier.startsWith("node:") || module.builtinModules.includes(specifier)) {
      const url = specifier.startsWith("node:") ? specifier : `node:${specifier}`;
      return { url, shortCircuit: true };
    }
    if (specifier.startsWith("data:")) return { url: specifier, shortCircuit: true };
    // A relative/bare import from inside nub's graph: resolve it natively from the
    // parent's own require() resolver (NOT nub's tsconfig/clobber/probe logic) and
    // short-circuit. Bare specifiers resolve from the parent package's location.
    try {
      const parentReq = createRequire(parentURL);
      const resolved = parentReq.resolve(specifier);
      return { url: pathToFileURL(resolved).href, shortCircuit: true };
    } catch {
      // Couldn't resolve from the parent (e.g. a non-file: parent): still short-
      // circuit by handing the specifier back as-is, so the user chain is bypassed.
      return null;
    }
  }

  // node: and data: protocols, and bare Node built-ins, are never ours.
  if (specifier.startsWith("node:") || specifier.startsWith("data:")) return null;
  if (module.builtinModules.includes(specifier)) return null;

  // 1. Built-in modules provided by Nub.
  if (BUILTIN_MODULES.has(specifier)) {
    return { url: BUILTIN_MODULES.get(specifier), shortCircuit: true };
  }

  // 2. Vendored packages (e.g. @oxc-project/runtime).
  const bare = barePkg(specifier);
  if (VENDORED_PACKAGES.has(bare)) {
    const resolved = safeRequireResolve(specifier);
    if (resolved) return { url: pathToFileURL(resolved).href, shortCircuit: true };
  }

  // 3. Package clobbering.
  if (CLOBBER_MAP.has(bare) && !isNodeModules(parentURL || "")) {
    return { url: `data:text/javascript,${encodeURIComponent(CLOBBER_MAP.get(bare)())}`, shortCircuit: true };
  }

  const parent = String(parentURL || "");
  const parentExt = extname(parent);

  // 4. tsconfig-paths (only for bare/aliased specifiers, not relative).
  if (!specifier.startsWith(".") && !specifier.startsWith("/") && !specifier.startsWith("file:") && !isNodeModules(parent)) {
    const parentDir = parent.startsWith("file:") ? dirname(fileURLToPath(parent)) : process.cwd();
    const { matcher } = getTsconfigForDir(parentDir);
    if (matcher) {
      const mapped = matcher(specifier);
      if (mapped && mapped.length > 0) {
        for (const candidate of mapped) {
          const resolved = tryResolveFile(candidate, parentExt);
          if (resolved) return { url: pathToFileURL(resolved).href, shortCircuit: true };
        }
      }
    }
  }

  // 5. Extensionless probing (only when parent is a TS file).
  if (TS_PARENT_EXTS.has(parentExt) && (specifier.startsWith("./") || specifier.startsWith("../"))) {
    const parentDir = dirname(fileURLToPath(parent));
    const target = pathResolve(parentDir, specifier);
    const resolved = tryResolveFile(target, parentExt);
    if (resolved) return { url: pathToFileURL(resolved).href, shortCircuit: true };
  }

  return null;
}

// CommonJS `require()` resolution for the compat-tier Module._resolveFilename
// patch. Returns an absolute file path for a require specifier nub should
// redirect (tsconfig `paths`, extensionless `.ts`, `.js`→`.ts` swap), or null to
// defer to Node's resolver. Mirrors resolveSpec steps 4–5 but returns a path (not
// a URL) and never handles clobber/vendored/builtin — those are import-only, and
// a clobber's data: URL can't be a require target. `parentPath` is the requiring
// file's absolute path (from the CJS parent Module), or null for the entry.
export function resolveCjsPath(request, parentPath) {
  if (request.startsWith("node:") || request.startsWith("data:") ||
      module.builtinModules.includes(request)) {
    return null;
  }
  const parentExt = parentPath ? pathExtname(parentPath) : "";

  // tsconfig `paths` — bare/aliased specifiers from a file outside node_modules
  // (not gated on a TS parent: a plain .js with a paths alias resolves too).
  if (!request.startsWith(".") && !request.startsWith("/") && !request.startsWith("file:") &&
      !isNodeModules(parentPath || "")) {
    const parentDir = parentPath ? dirname(parentPath) : process.cwd();
    const { matcher } = getTsconfigForDir(parentDir);
    if (matcher) {
      const mapped = matcher(request);
      if (mapped && mapped.length > 0) {
        for (const candidate of mapped) {
          const resolved = tryResolveFile(candidate, parentExt);
          if (resolved) return resolved;
        }
      }
    }
    return null; // a plain bare package → let Node resolve it from node_modules
  }

  // Extensionless probing + .js→.ts swap for a relative specifier — only when the
  // requiring file is itself TS (same TS_PARENT_EXTS gate as resolveSpec step 5).
  if (parentPath && TS_PARENT_EXTS.has(parentExt) &&
      (request.startsWith("./") || request.startsWith("../"))) {
    const target = pathResolve(dirname(parentPath), request);
    const resolved = tryResolveFile(target, parentExt);
    if (resolved) return resolved;
  }

  return null;
}

// Would `require()`-ing this resolved TS file need Node's require(esm)? An
// ESM-syntax `.ts`/`.mts` (or a `.ts` in a `type: module` package) transpiles to
// ESM, which `require()` can only load via require(esm). On the compat tier that
// path is the loader-worker's CJS translator, which on Node below the #60380 fix
// crashes cryptically (`cjsCache.get(job.url)` is undefined) instead of erroring.
// The compat CJS shim calls this so it can surface a clean ERR_REQUIRE_ESM
// instead. (`.cts` is always CommonJS → false; non-transpiled extensions → false.)
export function requireTargetIsEsm(filePath, ext) {
  if (ext === ".cts") return false;
  if (ext === ".mts") return true;
  if (!TRANSPILE_EXTS.has(ext)) return false;
  let source;
  try { source = readFileSync(filePath, "utf8"); } catch { return false; }
  const pkgType = getPackageType(dirname(filePath));
  return moduleFormatFor(ext, pkgType, filePath, source) === "module";
}

// ── Module-format detection ─────────────────────────────────────────
// oxc-parser loads a native binding (~8 ms) and is needed ONLY for absent-`type`
// module detection + the Stage-3 decorator guard, so it loads lazily — `nub
// script.js` and explicit-`type` files never pay for it.
//
// oxc-parser is ESM-only, so `require()` of it needs require(esm), which only
// exists on Node 20.19+ / 22.12+. The fast tier (>= 22.15) has it, so a lazy
// `require` there keeps the load off plain-JS startups. The compat tier reaches
// down to 18.19, where `require("oxc-parser")` throws ERR_REQUIRE_ESM — so the
// compat-tier callers (the loader worker's async load hook, and preload.mjs's
// main-thread compat branch) `await ensureParser()` first, which loads it via
// dynamic `import()` (native ESM, works on every supported Node). Without this,
// detection silently fell back to "ESM", so a CJS-content `.ts` mis-loaded as ESM
// and decorator syntax slipped past the guard on Node 18.19 / 20.x.
let _parseSync = null;
let _requireTried = false;
let _importTried = false;

/// Async, idempotent: ensure oxc-parser is loaded via dynamic import (the only
/// form that works below require(esm)). Compat-tier callers await this before the
/// synchronous detection below runs.
export async function ensureParser() {
  if (_parseSync || _importTried) return;
  _importTried = true;
  try { _parseSync = (await import("oxc-parser")).parseSync; } catch { /* unavailable */ }
}

function getParseSync() {
  if (!_parseSync && !_requireTried) {
    _requireTried = true;
    try { _parseSync = __require("oxc-parser").parseSync; } catch { /* try ensureParser instead */ }
  }
  return _parseSync;
}

// Does the source carry VALUE-level ESM syntax? Mirrors Node's
// `--experimental-detect-module`: type-only imports/exports are erased by oxc
// and must NOT count, but a value import/export, a bare `import "x"`, an
// `export {}` marker, `import.meta`, or top-level `await` all force ESM.
// Used only for the ambiguous extensions when package.json has no `type`.
function hasEsmSyntax(filePath, source, lang) {
  const parse = getParseSync();
  if (!parse) return true; // detection unavailable → default ESM (the common case)
  let mod;
  try {
    mod = parse(filePath, source, { lang }).module;
  } catch {
    return false; // unparseable → default CJS; the transpile surfaces the real error
  }
  const valueImport = mod.staticImports.some(
    (si) => si.entries.length === 0 || si.entries.some((e) => !e.isType),
  );
  const valueExport = mod.staticExports.some(
    (se) => se.entries.length === 0 || se.entries.some((e) => !e.isType),
  );
  if (valueImport || valueExport || mod.importMetas.length > 0) return true;
  // Top-level await: `hasModuleSyntax` is set with no static import/export/meta.
  return (
    mod.hasModuleSyntax &&
    mod.staticImports.length === 0 &&
    mod.staticExports.length === 0 &&
    mod.importMetas.length === 0
  );
}

// Map a transpiled file's extension + nearest package.json "type" to the module
// format Node's loader should use. `.mts`/`.cts` are explicit; an explicit
// `type` is authoritative; otherwise (ambiguous) we detect from source syntax —
// full Node parity (`--experimental-detect-module`), so a CJS-syntax `.ts` with
// no `type` runs as CJS on nub exactly as on Node. See wiki/runtime/module-format.md.
export function moduleFormatFor(ext, pkgType, filePath, source) {
  if (ext === ".mts") return "module";
  if (ext === ".cts") return "commonjs";
  if (pkgType === "module") return "module";
  if (pkgType === "commonjs") return "commonjs";
  const lang = ext === ".tsx" ? "tsx" : ext === ".jsx" ? "jsx" : "ts";
  return hasEsmSyntax(filePath, source, lang) ? "module" : "commonjs";
}

// The Stage-3-decorator rejection diagnostic. oxc does not lower TC39 Stage 3
// decorators yet (oxc-project/oxc#9170) — it passes the `@decorator` syntax
// through verbatim with errors:[], so without this check V8 throws a bare
// `SyntaxError: Invalid or unexpected token`. See wiki/runtime/stage3-decorators.md.
function stage3DecoratorError(filePath) {
  return new Error(
    `Nub: Stage 3 decorators are not supported by the transpiler yet.\n` +
    `This is an upstream limitation in oxc (oxc-project/oxc#9170).\n` +
    `  in ${filePath}\n\n` +
    `Workarounds:\n` +
    `  1. Set "experimentalDecorators": true in tsconfig.json to use legacy decorators\n` +
    `     (the shape NestJS / TypeORM / class-validator are written against).\n` +
    `  2. Wait for Stage 3 decorator support in oxc; tracked upstream at\n` +
    `     https://github.com/oxc-project/oxc/issues/9170.\n\n` +
    `See: https://www.typescriptlang.org/tsconfig/#experimentalDecorators`,
  );
}

// Does the source contain TC39 decorator syntax (`@expr` on a class or class
// member)? Used ONLY when legacy decorators are off, to surface a clear
// diagnostic instead of oxc's verbatim passthrough → V8 SyntaxError. The cheap
// `source.includes("@")` pre-filter in the caller keeps decorator-free files off
// the parser; decorators only attach to ClassDeclaration/ClassExpression and
// their members (incl. accessors), and to `export`/`export default` wrappers.
function hasDecoratorSyntax(filePath, source, lang) {
  const parse = getParseSync();
  if (!parse) return false; // parser unavailable → let oxc/V8 surface the error
  let program;
  try {
    program = parse(filePath, source, { lang }).program;
  } catch {
    return false; // unparseable → the transpile/V8 surfaces the real error
  }
  let found = false;
  const visit = (node) => {
    if (found || !node || typeof node !== "object") return;
    if (Array.isArray(node)) {
      for (const child of node) visit(child);
      return;
    }
    if (Array.isArray(node.decorators) && node.decorators.length > 0) {
      found = true;
      return;
    }
    for (const k in node) {
      if (k === "type" || k === "start" || k === "end") continue;
      visit(node[k]);
      if (found) return;
    }
  };
  visit(program.body);
  return found;
}

// Drop a trailing bare `export {};` — oxc injects it to preserve module-ness
// after stripping a file's only module syntax (e.g. a lone `import type`).
const EMPTY_EXPORT_MARKER = /(?:^|\n)[ \t]*export[ \t]*\{[ \t]*\}[ \t]*;?\s*$/;
function stripEmptyExportMarker(code) {
  return code.replace(EMPTY_EXPORT_MARKER, "");
}

// ── Transpile cache ─────────────────────────────────────────────────
// NUB_VERSION (from version.mjs) is the SOLE version component of the cache key:
// oxc-transform is pinned exact + vendored per release, so any emit change ships
// only in a new nub version, which `make version` bumps. CACHE_SCHEMA busts the
// cache when the on-disk ENTRY FORMAT changes (v3 = integrity prefix + leading
// format byte). The fast and compat tiers share this cache: post-extraction they
// emit byte-identical output for the same (source, ext, tsconfig, pkgType), so a
// single cache under one key is correct and maximizes hits.
const CACHE_SCHEMA = "3";
// Disable the transpile cache when (a) the permission model is active (writing a
// cache file may not be granted), or (b) the user set `NODE_COMPILE_CACHE=0` —
// Node's compile-cache disable signal, which nub honors as "no caching in this
// pipeline" (one knob for both V8's compile cache and nub's transpile cache; no
// nub-specific env var). Per wiki/runtime/transpile-cache.md (Colin 2026-05-18).
const CACHE_DISABLED =
  process.permission?.has !== undefined || process.env.NODE_COMPILE_CACHE === "0";
let cacheDir = null;
if (!CACHE_DISABLED) {
  const base = process.env.XDG_CACHE_HOME || (process.env.HOME ? join(process.env.HOME, ".cache") : null);
  if (base) {
    cacheDir = join(base, "nub", "transpile");
    try { mkdirSync(cacheDir, { recursive: true }); } catch { cacheDir = null; }
  }
}

function cacheKey(source) {
  return getCreateHash()("sha256")
    .update(NUB_VERSION).update("\0")
    .update(CACHE_SCHEMA).update("\0")
    .update(source)
    .digest("hex");
}
// Each entry is stored as `<16-hex integrity prefix><body>`, where the prefix is
// the first 8 bytes of sha256(body). cacheGet re-checks it and treats ANY
// mismatch — truncation, on-disk corruption, bit-rot, external edits — as a miss,
// so the entry is re-transpiled and overwritten (self-heals) instead of feeding
// garbage to V8.
const CACHE_INTEGRITY_LEN = 16;
function cacheIntegrity(body) {
  return getCreateHash()("sha256").update(body).digest("hex").slice(0, CACHE_INTEGRITY_LEN);
}
function cacheGet(key) {
  if (!cacheDir) return null;
  let raw;
  try {
    raw = readFileSync(join(cacheDir, key), "utf8");
  } catch {
    return null;
  }
  if (raw.length < CACHE_INTEGRITY_LEN) return null;
  const body = raw.slice(CACHE_INTEGRITY_LEN);
  if (raw.slice(0, CACHE_INTEGRITY_LEN) !== cacheIntegrity(body)) return null;
  return body;
}
let cacheTmpCounter = 0;
function cacheSet(key, value) {
  if (!cacheDir) return;
  const finalPath = join(cacheDir, key);
  // Atomic write: temp file in the same dir, then rename (atomic on POSIX +
  // Windows same-volume), so a concurrent reader sees old-or-complete, never torn.
  const tmpPath = `${finalPath}.${process.pid}.${cacheTmpCounter++}.tmp`;
  try {
    writeFileSync(tmpPath, cacheIntegrity(value) + value);
    renameSync(tmpPath, finalPath);
  } catch {
    try { unlinkSync(tmpPath); } catch {}
  }
}

// ── Bounded-cache maintenance ───────────────────────────────────────
const CACHE_MAX_BYTES = 512 * 1024 * 1024; // 512 MiB — bounds runaway growth, not normal use
const SWEEP_INTERVAL_MS = 24 * 60 * 60 * 1000; // ≤ one sweep per day
export function maybeSweepCache() {
  if (!cacheDir) return;
  // Workers inherit this preload (via execArgv); only the main thread sweeps.
  try {
    if (!__require("node:worker_threads").isMainThread) return;
  } catch {
    return;
  }
  const sentinel = join(cacheDir, ".sweep");
  const s = statSync(sentinel, { throwIfNoEntry: false });
  if (s && Date.now() - s.mtimeMs < SWEEP_INTERVAL_MS) return;
  try {
    writeFileSync(sentinel, "");
  } catch {
    return;
  }
  import("./cache-evict.mjs")
    .then((m) => m.sweepCache(cacheDir, CACHE_MAX_BYTES))
    .catch(() => {});
}

// ── Transpile ───────────────────────────────────────────────────────
// Transpile a TS/JSX file to JS, returning `{ format, source, shortCircuit }` in
// the shape both hook tiers hand back to Node. Format is detected (not derived
// from extension alone), so a CommonJS-syntax `.ts` is reported `commonjs` — the
// fix that makes `require()` of a TS file work on the compat tier, where Node's
// CJS translator loads it via this hook and keys on the returned format.
export function loadTranspile(url, ext) {
  const filePath = fileURLToPath(url);
  const source = readFileSync(filePath, "utf8");
  const dir = dirname(filePath);
  const { tsconfig } = getTsconfigForDir(dir);
  const co = tsconfig?.config?.compilerOptions;

  // Cache key folds in ext, the resolved tsconfig, and the nearest package.json
  // type — the same source can transpile to a different format under a different
  // type. The cached entry's leading byte ('c'/'m') records the chosen format,
  // so a hit needs no re-detection.
  const pkgType = ext === ".mts" || ext === ".cts" ? undefined : getPackageType(dir);
  const tsconfigHash = co ? JSON.stringify(co) : "";
  const key = cacheKey(source + "\0" + ext + "\0" + tsconfigHash + "\0" + (pkgType || ""));
  const cached = cacheGet(key);
  if (cached) {
    return {
      format: cached[0] === "c" ? "commonjs" : "module",
      source: cached.slice(1),
      shortCircuit: true,
    };
  }

  const format = moduleFormatFor(ext, pkgType, filePath, source);

  const lang = ext === ".tsx" ? "tsx" : ext === ".jsx" ? "jsx" : "ts";
  const opts = {
    lang,
    sourceType: format === "commonjs" ? "commonjs" : "module",
    sourcemap: true,
    // Lower syntax newer than the 22.15 floor. Critically this downlevels
    // `using`/`await using` (Explicit Resource Management) — unparseable on Node
    // 22's V8 — into the vendored `@oxc-project/runtime/helpers/usingCtx` shape,
    // which resolves via VENDORED_PACKAGES. Without a target, oxc leaves `using`
    // verbatim and Node 22 throws a SyntaxError. es2022 is the highest target
    // that still lowers `using` while leaving everything Node 22 already supports
    // (top-level await, class fields, private methods) untouched.
    target: "es2022",
    typescript: {},
    // Decorators default to OFF (Stage-3 mode), matching tsc: legacy semantics
    // and metadata are opt-in via tsconfig. See wiki/runtime/non-erasable-syntax.md.
    decorator: co?.experimentalDecorators === true
      ? { legacy: true, emitDecoratorMetadata: co?.emitDecoratorMetadata === true }
      : undefined,
  };
  if (ext === ".tsx" || ext === ".jsx") {
    opts.jsx = {
      runtime: co?.jsx === "react" ? "classic" : "automatic",
      importSource: co?.jsxImportSource || "react",
    };
    if (co?.jsxFactory) opts.jsx.pragma = co.jsxFactory;
    if (co?.jsxFragmentFactory) opts.jsx.pragmaFrag = co.jsxFragmentFactory;
  }

  // Stage-3 decorators: oxc returns errors:[] and emits the `@decorator` syntax
  // verbatim, so the result-error check below never fires and V8 throws a bare
  // SyntaxError. When legacy mode is off and decorator syntax is present, reject
  // with the documented Option-A diagnostic instead.
  if (co?.experimentalDecorators !== true && source.includes("@") &&
      hasDecoratorSyntax(filePath, source, lang)) {
    throw stage3DecoratorError(filePath);
  }

  const result = transformSync(filePath, source, opts);
  if (result.errors.length > 0) {
    const details = result.errors.map((e) => e.codeframe || e.message).join("\n\n");
    throw new Error(`Transpile error in ${filePath}:\n${details}`);
  }

  let code = result.code;
  // A CommonJS file must not carry oxc's injected ESM `export {};` marker (CJS
  // body + ESM marker won't run). Node's strip-types emits no such marker.
  if (format === "commonjs") code = stripEmptyExportMarker(code);
  if (result.map) {
    const map = typeof result.map === "string" ? JSON.parse(result.map) : result.map;
    map.sourcesContent = [source];
    code += `\n//# sourceMappingURL=data:application/json;base64,${Buffer.from(JSON.stringify(map)).toString("base64")}\n`;
  }
  // Append a `//# sourceURL=` magic comment, matching Node's native strip-types
  // (lib/internal/modules/typescript.js: `return ${code}\n\n//# sourceURL=${filename}`).
  // This is the marker V8/the inspector reads to set `scriptParsed.hasSourceURL =
  // true` — the signal that a script is generated/transpiled rather than read
  // verbatim from disk (test-inspector-strip-types asserts it). It coexists with
  // the inline sourceMappingURL above (maps still drive stack frames); sourceURL
  // only names the origin. Use the absolute file path, exactly as Node does.
  code += `\n//# sourceURL=${filePath}\n`;

  // Store the chosen format as a leading byte so cache hits skip re-detection.
  cacheSet(key, (format === "commonjs" ? "c" : "m") + code);
  return { format, source: code, shortCircuit: true };
}

// ── Data-format imports ─────────────────────────────────────────────
function lazyRequire(pkg) {
  try { return __require(pkg); } catch {
    throw new Error(`Nub: importing this file requires the "${pkg}" package.\nInstall it: npm install ${pkg}`);
  }
}

function stripJsonComments(text) {
  let result = "", i = 0, inString = false, escape = false;
  while (i < text.length) {
    const ch = text[i];
    if (escape) { result += ch; escape = false; i++; continue; }
    if (inString) { if (ch === "\\") escape = true; if (ch === '"') inString = false; result += ch; i++; continue; }
    if (ch === '"') { inString = true; result += ch; i++; continue; }
    if (ch === "/" && text[i + 1] === "/") { while (i < text.length && text[i] !== "\n") i++; continue; }
    if (ch === "/" && text[i + 1] === "*") { i += 2; while (i < text.length && !(text[i] === "*" && text[i + 1] === "/")) i++; i += 2; continue; }
    result += ch; i++;
  }
  return result;
}

export function loadData(url, ext) {
  const filePath = fileURLToPath(url);
  const raw = readFileSync(filePath, "utf8");
  const kind = DATA_EXTS[ext];

  if (kind === "txt") {
    return { format: "module", source: `export default ${JSON.stringify(raw)};\n`, shortCircuit: true };
  }

  let parsed;
  if (nubNative) {
    if (kind === "yaml") parsed = nubNative.parseYaml(raw);
    else if (kind === "toml") parsed = nubNative.parseToml(raw);
    else if (kind === "json5") parsed = nubNative.parseJson5(raw);
    else if (kind === "jsonc") parsed = nubNative.parseJsonc(raw);
  } else {
    if (kind === "yaml") parsed = lazyRequire("yaml").parse(raw);
    else if (kind === "toml") parsed = lazyRequire("@iarna/toml").parse(raw);
    else if (kind === "json5") parsed = lazyRequire("json5").parse(raw);
    else if (kind === "jsonc") parsed = JSON.parse(stripJsonComments(raw));
  }

  if (parsed == null) {
    return { format: "module", source: "export default undefined;\n", shortCircuit: true };
  }

  let code = `const _data = ${JSON.stringify(parsed)};\nexport default _data;\n`;
  if (typeof parsed === "object" && !Array.isArray(parsed)) {
    for (const key of Object.keys(parsed)) {
      // Emit a named export only for keys that are valid, non-reserved binding
      // identifiers; everything else remains reachable via the default export.
      if (/^[a-zA-Z_$][a-zA-Z0-9_$]*$/.test(key) && !RESERVED_EXPORT_NAMES.has(key)) {
        code += `export const ${key} = _data[${JSON.stringify(key)}];\n`;
      }
    }
  }
  return { format: "module", source: code, shortCircuit: true };
}
