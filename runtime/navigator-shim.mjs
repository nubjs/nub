// `navigator` global backfill for Node < 21 (where the global is wholly absent).
//
// Node ships `globalThis.navigator` from 21.0.0. Below that it is `undefined`, so
// any web-platform API hosted ON navigator — chiefly `navigator.locks` (Web Locks,
// see navigator-locks.mjs) — has no object to attach to and silently does nothing.
// nub's floor is 18.19, so to make those APIs reach the floor we synthesize the
// `navigator` object Node 21+ would provide. Companion to navigator-locks.mjs:
// this MUST run BEFORE it so locks has a host on 18.19–20.x.
//
// Shape mirrors Node's internal/navigator.js: a `Navigator` instance with the
// enumerable prototype getters `hardwareConcurrency`, `language`, `languages`,
// `userAgent`, `platform` (locks is added separately by navigator-locks.mjs). The
// userAgent is `Node.js/<major>` — NEVER `Nub/…`: the user is running Node, nub is
// the augmenter, and a `Nub/` UA would be a brand-boundary leak.
//
// VERSION-GATE, not a global read: installNavigatorShim() returns early on Node >= 21
// from `process.versions.node` WITHOUT ever touching `globalThis.navigator`. That
// matters because on Node 24.5+ the native `navigator` is a lazy getter whose first
// access realizes ~30 internal/stream/worker-io builtins — a cold-start regression
// (test-bootstrap-modules). The shim must never trigger that; the version check makes
// the fast tier (>= 22.15, navigator always present) a free no-op.
//
// node: builtins (node:os) are fetched via `process.getBuiltinModule` when present,
// else via a createRequire THREADED IN through `setBootstrapCreateRequire` — the same
// brand-safe, off-the-user-loader-chain pattern worker-polyfill.mjs uses. The narrow
// floor (18.19.x, 20.11–20.15) lacks `process.getBuiltinModule`, and the shim only
// touches os lazily (the `hardwareConcurrency` getter), so the threaded require is
// needed exactly there. `os` is never loaded on Node >= 21 (the early return).

let _bootstrapCreateRequire = null;
export function setBootstrapCreateRequire(fn) {
  _bootstrapCreateRequire = fn;
}

function __getBuiltin(id) {
  if (typeof process.getBuiltinModule === "function") return process.getBuiltinModule(id);
  if (_bootstrapCreateRequire) return _bootstrapCreateRequire(import.meta.url)(id);
  // Last-resort: a bare specifier require off this module's own createRequire is
  // unavailable in ESM without node:module, which is exactly what the threading
  // avoids importing statically. If neither path is wired, surface a clear error.
  throw new Error("navigator-shim: no builtin accessor for " + id);
}

function deriveLanguage() {
  // Approximate Node's ICU default locale from the POSIX locale env, falling back
  // to "en-US" (Node's own fallback). `en_US.UTF-8` → `en-US`.
  const raw =
    process.env.LC_ALL || process.env.LC_MESSAGES || process.env.LANG || "";
  const base = raw.split(".")[0].split("@")[0].replace("_", "-");
  return base && base !== "C" && base !== "POSIX" ? base : "en-US";
}

function navigatorPlatform(platform, arch) {
  // Mirror node/lib/internal/navigator.js getNavigatorPlatform.
  if (platform === "darwin") return "MacIntel";
  if (platform === "win32") return "Win32";
  if (platform === "linux") {
    if (arch === "ia32") return "Linux i686";
    if (arch === "x64") return "Linux x86_64";
    return `Linux ${arch}`;
  }
  if (platform === "freebsd") return arch === "ia32" ? "FreeBSD i386" : arch === "x64" ? "FreeBSD amd64" : `FreeBSD ${arch}`;
  if (platform === "openbsd") return arch === "ia32" ? "OpenBSD i386" : arch === "x64" ? "OpenBSD amd64" : `OpenBSD ${arch}`;
  if (platform === "sunos") return arch === "ia32" ? "SunOS i86pc" : `SunOS ${arch}`;
  if (platform === "aix") return "AIX";
  return `${platform[0].toUpperCase()}${platform.slice(1)} ${arch}`;
}

// Build the Navigator class lazily (only when actually backfilling) so loading this
// module on the fast tier costs nothing beyond the early-return check.
function makeNavigator() {
  let _hw, _lang, _langs, _ua, _plat;
  class Navigator {
    get hardwareConcurrency() {
      // os.availableParallelism honors cgroup CPU limits (Node 18.14+/19.4+; present
      // on the 18.19 floor); fall back to cpus().length on the off chance it's absent.
      if (_hw === undefined) {
        const os = __getBuiltin("node:os");
        _hw = typeof os.availableParallelism === "function" ? os.availableParallelism() : os.cpus().length;
      }
      return _hw;
    }
    get language() {
      return (_lang ??= deriveLanguage());
    }
    get languages() {
      return (_langs ??= Object.freeze([this.language]));
    }
    get userAgent() {
      // `Node.js/<major>` — match Node exactly; never a nub-branded UA.
      return (_ua ??= `Node.js/${process.versions.node.split(".")[0]}`);
    }
    get platform() {
      return (_plat ??= navigatorPlatform(process.platform, process.arch));
    }
  }
  // Match Node's instance shape: the property getters live on the prototype and are
  // ENUMERABLE (Node sets them with kEnumerableProperty), so `for (const k in
  // navigator)` walks them while `Object.keys(navigator)` (own enumerable) is empty.
  // Class-body getters default to enumerable:false, so flip them in place.
  for (const k of ["hardwareConcurrency", "language", "languages", "userAgent", "platform"]) {
    Object.defineProperty(Navigator.prototype, k, { enumerable: true });
  }
  return { Navigator, instance: new Navigator() };
}

// Install the navigator backfill if and only if the running Node lacks the global
// (i.e. Node < 21). Idempotent and side-effect-free on Node >= 21.
export function installNavigatorShim() {
  const major = parseInt(process.versions.node.split(".")[0], 10);
  // Node 21+ ships navigator natively — never read the (possibly lazy) global here.
  if (major >= 21) return;
  // Defensive idempotency for the < 21 path (navigator is genuinely undefined there,
  // so this read can't trigger a lazy realization).
  if (typeof globalThis.navigator !== "undefined") return;

  const { Navigator, instance } = makeNavigator();

  // The binding on globalThis is NON-ENUMERABLE (invisible to Object.keys(globalThis)
  // / for-in) — nub's additive-global contract, matching how reportError and Worker
  // are installed. Configurable + writable so user code may still override it.
  Object.defineProperty(globalThis, "navigator", {
    value: instance,
    enumerable: false,
    writable: true,
    configurable: true,
  });
  // Node also exposes the `Navigator` constructor globally; mirror it (non-enumerable).
  if (typeof globalThis.Navigator === "undefined") {
    Object.defineProperty(globalThis, "Navigator", {
      value: Navigator,
      enumerable: false,
      writable: true,
      configurable: true,
    });
  }
}

// Fast tier / modern compat (getBuiltinModule present, Node >= 20.16): install eagerly
// at module eval — a guaranteed no-op above Node 21 thanks to the version gate, and
// correct on a 20.16–20.x compat run where navigator is still absent. On the narrow
// floor below getBuiltinModule the compat entry calls setBootstrapCreateRequire(...)
// + installNavigatorShim() explicitly (mirroring worker-polyfill's wiring).
if (typeof process.getBuiltinModule === "function") installNavigatorShim();
