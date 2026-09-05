// Standalone Nub loader — the arming logic behind `node --import <pkg>` /
// `node --require <pkg>`, consumed the way tsx/ts-node are. Slim by design: it
// arms ONLY the resolve + transpile surface (TS/JSX/`using`-lowering, tsconfig
// `paths`, extension probing, data-format imports) from the shared
// transform-core / preload-common machinery, and none of the CLI runtime's
// process augmentation — no polyfills, no Temporal/Worker/navigator globals, no
// watch IPC, no user preload chain, no version marker. A file that runs under
// `node --import <pkg>` must behave identically minus TS-just-works.
//
// Import order is load-bearing (ESM evaluates imports in source order):
//   1. loader-addon-env.mjs — resolves the per-platform addon package and sets
//      the internal `__NUB_ADDON_PATH` plumbing var BEFORE transform-core's
//      module body probes for the addon.
//   2. floor-builtin.mjs — threads `createRequire` into transform-core on the
//      narrow pre-`process.getBuiltinModule` floor (18.19.x, 20.11–20.15,
//      22.0–22.2); a no-op elsewhere.
//   3. transform-core.mjs — the tier-agnostic resolve+transpile core, shared
//      verbatim with the nub CLI. It has ZERO static imports by construction, so
//      routing it through a user's loader chain leaks nothing (R11).
import "./loader-addon-env.mjs";
import { createRequire } from "./floor-builtin.mjs";
import * as core from "./transform-core.mjs";

const __require = createRequire(import.meta.url);
const module_ = __require("node:module");
const { fileURLToPath } = __require("node:url");
const { dirname, isAbsolute, resolve: resolvePath, sep } = __require("node:path");
const common = __require("./preload-common.cjs");

// One arming record per module instance (= per realm: the main thread and each
// user worker thread evaluate this module separately via inherited execArgv).
// `esmMode` records which hook surface the ESM side took, because the CJS side's
// classic-transpile decision depends on it.
const armed = { esm: false, cjs: false, esmMode: null };

const OWN_DIR = dirname(fileURLToPath(import.meta.url)) + sep;

// The loader package's own published name, for recognizing our own `--import`
// token in the foreign-loader scan. In the published package this file sits next
// to package.json; in the dev tree (runtime/) there is none, and path-prefix
// matching covers that case.
const OWN_PKG_NAME = (() => {
  try {
    const raw = __require("node:fs").readFileSync(
      fileURLToPath(new URL("./package.json", import.meta.url)),
      "utf8",
    );
    const name = JSON.parse(raw).name;
    return typeof name === "string" && name.length > 0 ? name : null;
  } catch {
    return null;
  }
})();

// nub's own preload chainer rides `--import` too; same marker preload-common uses.
const NUB_CHAIN_MARKER = /[\\/]\.nub[\\/]preload-chain\./;

// Is this `--import`/`--loader` value one of OUR OWN entrypoints (or nub's
// chainer), as opposed to a genuinely foreign async loader (tsx, ts-node, an OTel
// attach)? The distinction preload-common's own scan does not need to make — the
// CLI's fast tier is delivered by `--require`, so for it ANY `--import` is
// foreign — but the standalone loader IS an `--import`, so a value-blind scan
// would classify the loader itself as foreign and force the async tier on every
// run in the broken-compose band.
function isOwnLoaderToken(value) {
  if (!value) return false;
  if (NUB_CHAIN_MARKER.test(value)) return true;
  if (OWN_PKG_NAME && (value === OWN_PKG_NAME || value.startsWith(`${OWN_PKG_NAME}/`))) {
    return true;
  }
  try {
    const p = value.startsWith("file:") ? fileURLToPath(value) : value;
    if (isAbsolute(p) && (resolvePath(p) + sep).startsWith(OWN_DIR)) return true;
    // A relative dev-tree form (`--import ./runtime/loader-register.mjs`)
    // resolves from the CWD, matching how Node resolved it.
    if (p.startsWith(".") && (resolvePath(process.cwd(), p) + sep).startsWith(OWN_DIR)) {
      return true;
    }
  } catch {
    // Unparseable value — treat as foreign; over-selection of the async tier is
    // safe (it is always correct, just slower to start).
  }
  return false;
}

// A foreign async ESM loader riding THIS process's startup flags, via either
// delivery channel (execArgv or NODE_OPTIONS). Same two-channel scan as
// preload-common's computeForeignAsyncLoaderFlagPresent, but value-aware so our
// own token is excluded.
function foreignAsyncLoaderPresent() {
  const tokens = [];
  const argv = Array.isArray(process.execArgv) ? process.execArgv : [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (typeof a !== "string") continue;
    for (const flag of ["--import", "--loader", "--experimental-loader"]) {
      if (a === flag) {
        if (typeof argv[i + 1] === "string") tokens.push(argv[i + 1]);
      } else if (a.startsWith(`${flag}=`)) {
        tokens.push(a.slice(flag.length + 1));
      }
    }
  }
  const opts = process.env.NODE_OPTIONS;
  if (typeof opts === "string" && opts !== "") {
    const re = /(?:^|\s)--(?:experimental-)?(?:import|loader)(?:=|\s)("[^"]*"|\S*)/g;
    for (const match of opts.matchAll(re)) {
      tokens.push((match[1] || "").replace(/^"|"$/g, ""));
    }
  }
  return tokens.some((t) => t && !isOwnLoaderToken(t));
}

// Arm the loader. `esm` = the ESM hook surface (module.registerHooks on the fast
// tier, the module.register loader worker on the compat tier); `cjs` = the
// CommonJS require() surface (Module._resolveFilename + the classic transpile
// shim where the tier needs it). Idempotent per surface, so `--import <pkg>` plus
// `--require <pkg>/cjs` in one invocation arms each exactly once.
export function arm({ esm = true, cjs = true } = {}) {
  // Electron: the sync load hook deadlocks Electron's main-process module
  // bootstrap, and its app JS is pre-bundled (the bundler owns TS) — same bail,
  // same reason as the CLI preload (issue #246).
  if (process.versions.electron) return;

  const wantEsm = esm && !armed.esm;
  const wantCjs = cjs && !armed.cjs;
  if (!wantEsm && !wantCjs) return;

  const [major = 0, minor = 0] = process.versions.node
    .split(".")
    .map((n) => parseInt(n, 10));
  if (major < 18 || (major === 18 && minor < 19)) {
    process.stderr.write(
      `The Nub loader requires Node 18.19 or newer; got ${process.versions.node}. Hooks are inactive.\n`,
    );
    return;
  }

  // The loader ships no polyfill packages, so the clobber map's synthetic
  // modules (re-exports of globals the CLI runtime installs) would hand users
  // `undefined`. A user who installed @js-temporal/polyfill or urlpattern-polyfill
  // themselves must get the real package — clear the map before any hook runs.
  core.CLOBBER_MAP.clear();

  // No-op unless the nub CLI's watch mode spawned this process (it sets
  // WATCH_REPORT_DEPENDENCIES); wiring it keeps `nub watch` restarts correct when
  // the loader runs under it.
  const watchReporting = common.installWatchReporting(core);

  const hasSyncHooks = typeof module_.registerHooks === "function";
  // On 22.15.0–24.11.0 an async `module.register` loader's resolveSync/loadSync
  // are unimplemented stubs, so nub's sync hooks composing with a foreign async
  // loader (tsx, ts-node, an OTel ESM attach) would crash resolution. Register
  // via the async path there instead so both loaders compose all-async — the same
  // tier decision the CLI preload makes, minus counting ourselves as foreign.
  const foreignLoaderFlagPresent = foreignAsyncLoaderPresent();
  const forceAsync = common.nodeHookComposeBroken() && foreignLoaderFlagPresent;

  if (wantEsm) {
    if (hasSyncHooks && !forceAsync) {
      // The standalone --import is our own loader, not a foreign async hook.
      // Share that distinction with the import-of-CJS require.cache repair.
      const { resolve, load } = common.makeHooks(core, watchReporting, foreignLoaderFlagPresent);
      module_.registerHooks({ resolve, load });
      armed.esmMode = "sync";
    } else {
      // Compat tier (18.19–22.14, 23.0–23.4) or forced-async composition: hooks
      // run in a dedicated loader worker. It imports transform-core statically in
      // its own thread and finds the addon via the inherited __NUB_ADDON_PATH.
      // The data payload carries the clobber-map clear into that thread's OWN
      // transform-core instance — the clear() above only reaches this realm's.
      common.registerLoaderWorker("./preload-async-hooks.mjs", import.meta.url, {
        data: { standaloneLoader: true },
      });
      armed.esmMode = "worker";
    }
    armed.esm = true;
  }

  if (wantCjs) {
    // Classic require.extensions transpile is needed only where the sync
    // registerHooks load hook does not already transpile require()'d TS: on the
    // sync tier it does (and the classic shim would shadow native require(esm),
    // throwing bogus ERR_REQUIRE_ESM on ESM `.ts` — see preload.cjs); elsewhere
    // install it unless Node has native TypeScript (mirrors preload.mjs).
    const classic = armed.esmMode === "sync" ? false : !process.features?.typescript;
    common.installCjsRequireHooks(core, classic);
    armed.cjs = true;
  }

  // Bounded transpile-cache eviction, same cheap-probe shape as the CLI entries:
  // schedule the deferred sweep only on the once-a-day run where one is due.
  if (core.sweepDue()) {
    setImmediate(() => {
      try {
        core.maybeSweepCache();
      } catch {}
    });
  }
}
