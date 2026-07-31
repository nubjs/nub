// Windows build jail: give a confined Node a working `fs.realpath*`, and therefore a working
// module resolver.
//
// WHY THIS EXISTS. Node's JS `realpathSync` walks a path component by component and opens each
// prefix as a TARGET, starting with the volume root. The jail grants leaf-only and leans on
// bypass-traverse, which exempts INTERMEDIATE components of a single open — it does not make an
// ancestor openable as a target. So `lstat 'C:\'` is refused and every `require()` of an absolute
// path dies `EPERM`, on a file the same process can `readFileSync` in the same breath. Both native
// realpath surfaces are refused too (`GetFinalPathNameByHandleW`), so there is no working
// implementation to point at.
//
// THIS IS A PORT, NOT AN INVENTION. Microsoft shipped the same repair for the behaviourally
// identical IIS/shared-hosting case, where the app-pool identity can read the application
// directory and below but not the attributes of ancestor directories:
// `aspnet/JavaScriptServices#1102`, `PatchModuleResolutionLStat.ts`. Their predicate and their
// accepted tradeoff are carried over on `ancestorOfARoot` below.
//
// WHAT IT IS NOT. It is NOT `--preserve-symlinks`. That flag stops Node calling realpath at all,
// which under nub's default `Isolated` linker silently binds a DIFFERENT version of a package
// (`preserve_symlinks_isolated_layout`) — a lifecycle script that builds against the wrong
// dependency and exits 0. Here every component AT OR BELOW a jail root is still `lstat`'d for
// real and every symlink there is still read, so a store-cell link inside `node_modules` resolves
// to its true target.
//
// DELIVERY. An ESM module via `--import`, the same channel as `windows_stdio_shim.js` and
// `net_gate_shim.js`: `defaultResolve` short-circuits on `data:` before touching the filesystem,
// so the preload needs no grant and cannot be tampered with by the package it confines. `--import`
// runs INSIDE `executeUserEntryPoint`, i.e. AFTER `resolveMainPath` (`run_main.js`), so it cannot
// repair the entry point's own realpath — `--preserve-symlinks-main` covers that one, and is
// measured clean on the wrong-version axis (only the tree-wide flag moves dependency resolution).
// `--import file://…` is NOT interchangeable here: the preload's own resolution walks the same
// refused ancestors and EPERMs before it loads, so the `data:` channel is load-bearing.
//
// TWO LAYERS, AND BOTH ARE REQUIRED. Replacing the `fs.realpath*` PROPERTIES repairs CommonJS,
// because `helpers.js`'s `toRealPath` reads the property at call time. It does not repair ES
// modules: `internal/modules/esm/resolve.js` DESTRUCTURES `realpathSync` at module-load time on
// 18.19-24.16, 25.x and 26.0, and on 25.7-26.0 the resolver lives in the V8 startup snapshot,
// where no preload of any kind reaches it. So the repair is ALSO applied one layer down, at the
// `fs` binding — see `installBindingSeam`.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { getSystemErrorName } from "node:util";

// The jail's granted anchors, substituted by `windows_realpath_node_options`. A refused component
// is tolerated only if it is a strict ancestor of one of these.
const ROOTS = __NUB_REALPATH_ROOTS_JSON__;

// The force seam is what lets `realpath_shim_semantics` run on EVERY platform. What it repairs is
// a Windows fact; whether the walk reproduces Node's own resolution — symlinks, the per-component
// cache, the wrong-version control — is not, and a Windows-only test would leave the part most
// likely to break unmeasured on the machines people develop on. The value is a path-delimiter list
// of extra roots, so a fixture can anchor the walk on its own tmpdir. Inert without the preload.
const SENTINEL = "__nubJailRealpathShim";
const FORCE = process.env.__NUB_JAIL_REALPATH_SHIM_FORCE;
const forced = FORCE ? String(FORCE).split(path.delimiter).filter(Boolean) : [];
const roots = ROOTS.concat(forced);
if ((process.platform === "win32" || forced.length > 0) && !globalThis[SENTINEL] && roots.length) {
  globalThis[SENTINEL] = true;
  install(roots);
}

function install(roots) {
  const isWindows = process.platform === "win32";
  const isSep = isWindows ? (ch) => ch === "\\" || ch === "/" : (ch) => ch === "/";
  // Node's own `splitRoot`, which this walk must agree with or a UNC path desynchronises.
  const splitRoot = isWindows
    ? (s) => /^(?:[a-zA-Z]:|[\\/]{2}[^\\/]+[\\/][^\\/]+)?[\\/]*/.exec(s)[0]
    : (s) => {
        for (let i = 0; i < s.length; ++i) if (s[i] !== "/") return s.slice(0, i);
        return s;
      };
  const nextPart = (p, i) => {
    for (; i < p.length; ++i) if (isSep(p[i])) return i;
    return -1;
  };

  // The `\\?\` / `\\?\UNC\` prefixes are stripped for COMPARISON only — the walk still passes the
  // caller's own spelling to `lstat`. Without this a long-path spelling of a jailed path finds no
  // matching root and the tolerance never fires, which measured as `native-longpath-granted=ERR`
  // in an otherwise repaired arm (run 30513204884).
  const stripLongPrefix = (p) =>
    p.startsWith("\\\\?\\UNC\\") ? `\\\\${p.slice(8)}` : p.startsWith("\\\\?\\") ? p.slice(4) : p;
  const norm = (p) =>
    isWindows ? stripLongPrefix(path.resolve(p)).toLowerCase() : path.resolve(p);
  const normRoots = roots.map(norm);

  // MICROSOFT'S PREDICATE, generalised from one app root to the jail's several granted anchors:
  // `startsWith(appRootDirLong, pathToStatLong)` — the refused path must be a strict
  // path-boundary PREFIX of a root, i.e. an ancestor directory of something the jail granted.
  //
  // WHY THE ASSERTION IS SOUND HERE, AND WHY IT IS NOT `--preserve-symlinks`: the only components
  // this can cover are the ones above every grant — `C:\`, `C:\Users`, `%USERPROFILE%` — which are
  // real directories, never store-cell links. Everything at or below a root is `lstat`'d for real,
  // so every symlink that decides WHICH version of a package resolves is still followed, which is
  // exactly what the tree-wide flag gives up. Scoped rather than unconditional so a refusal on a
  // component inside the tree stays a loud error instead of a silently faked directory.
  const ancestorOfARoot = (p) => {
    const c = norm(p);
    // The trailing boundary clause is NOT redundant with `startsWith`: it is what keeps the match
    // on a path COMPONENT boundary, so `C:\foo` is not tolerated as an ancestor of a root
    // `C:\foobar\pkg`. Delete it and the tolerance fakes a directory for any sibling sharing a
    // name prefix with a grant — widening the rule past what the jail actually granted. Both
    // disjuncts are needed (a root spelled `C:\` ends in the separator itself); measured correct
    // against 16 adversarial shapes.
    return normRoots.some(
      (r) =>
        r.length > c.length &&
        r.startsWith(c) &&
        (isSep(r[c.length]) || isSep(c[c.length - 1])),
    );
  };

  // ERROR_ACCESS_DENIED reaches JS as either of these depending on the libuv path, so both mean
  // "the jail will not let me interrogate this" — and nothing else does.
  const refused = (e) => e && (e.code === "EPERM" || e.code === "EACCES");

  // Distinct from a Stats object so a tolerated component is never mistaken for a real one.
  const TOLERATED = Symbol("nub.toleratedAncestor");

  function lstatOrTolerate(target) {
    try {
      return fs.lstatSync(target);
    } catch (e) {
      if (refused(e) && ancestorOfARoot(target)) return TOLERATED;
      throw e;
    }
  }

  const toPathString = (p) => {
    if (typeof p === "string") return p;
    if (p instanceof URL) return fileURLToPath(p);
    if (Buffer.isBuffer(p)) return p.toString();
    return `${p}`;
  };
  const encodeResult = (result, options) => {
    const encoding = typeof options === "string" ? options : options && options.encoding;
    if (!encoding || encoding === "utf8") return result;
    const asBuffer = Buffer.from(result);
    return encoding === "buffer" ? asBuffer : asBuffer.toString(encoding);
  };
  // `toRealPath` passes Node's module-resolution cache under an internal symbol. Reading it back
  // by description keeps the memoisation the resolver relies on — without it every resolution
  // re-walks the whole path — and keeps this walk's entries usable by Node's own realpath.
  const cacheOf = (options) => {
    if (!options || typeof options !== "object") return undefined;
    for (const s of Object.getOwnPropertySymbols(options)) {
      if (s.description === "realpathCacheKey") {
        const c = options[s];
        if (c && typeof c.get === "function") return c;
      }
    }
    return undefined;
  };

  // Node's `lib/fs.js` `realpathSync` walk, re-expressed on public `fs.lstatSync`/`readlinkSync`
  // with the tolerance rule above: same algorithm, same cache protocol, same symlink-restart
  // semantics. The one divergence is what happens when a component cannot be interrogated.
  function walk(input, options) {
    let p = path.resolve(toPathString(input));
    const cache = cacheOf(options);
    const cached = cache && cache.get(p);
    if (cached) return encodeResult(cached, options);

    const original = p;
    const knownHard = new Set();
    const seenLinks = new Map();

    let current = splitRoot(p);
    let base = current;
    let pos = current.length;

    // Node probes the volume root for existence before walking, on Windows only. It carries no
    // semantic content for the caller (upstream's own comment says so, and Unix skips it) and it
    // is the measured `EPERM lstat 'C:\'`, so a refusal on a tolerated ancestor is absorbed here.
    if (isWindows) {
      lstatOrTolerate(base);
      knownHard.add(base);
    }

    while (pos < p.length) {
      const result = nextPart(p, pos);
      const previous = current;
      if (result === -1) {
        const last = p.slice(pos);
        current += last;
        base = previous + last;
        pos = p.length;
      } else {
        current += p.slice(pos, result + 1);
        base = previous + p.slice(pos, result);
        pos = result + 1;
      }

      if (knownHard.has(base) || (cache && cache.get(base) === base)) continue;

      let resolvedLink = cache && cache.get(base);
      if (!resolvedLink) {
        const stats = lstatOrTolerate(base);
        if (stats === TOLERATED || !stats.isSymbolicLink()) {
          knownHard.add(base);
          if (cache) cache.set(base, base);
          continue;
        }
        let linkTarget = null;
        let id;
        if (!isWindows) {
          id = `${stats.dev}:${stats.ino}`;
          if (seenLinks.has(id)) linkTarget = seenLinks.get(id);
        }
        if (linkTarget === null) {
          // Upstream stats the link before reading it so a dangling one surfaces as ENOENT.
          fs.statSync(base);
          linkTarget = fs.readlinkSync(base);
        }
        resolvedLink = path.resolve(previous, linkTarget);
        if (cache) cache.set(base, resolvedLink);
        if (!isWindows) seenLinks.set(id, linkTarget);
      }

      p = path.resolve(resolvedLink, p.slice(pos));
      current = base = splitRoot(p);
      pos = current.length;
      if (isWindows && !knownHard.has(base)) {
        lstatOrTolerate(base);
        knownHard.add(base);
      }
    }

    if (cache) cache.set(original, p);
    return encodeResult(p, options);
  }

  // Every surface, because under the jail every one of them is refused and a package reaching for
  // `.native` or the async form would otherwise still die. The substitutes lose the OS's own
  // canonical spelling (`.native` normalises drive-letter case); a working answer beats `EPERM`.
  const sync = (p, options) => walk(p, options);
  sync.native = sync;
  fs.realpathSync = sync;

  const async_ = (p, options, callback) => {
    const cb = typeof options === "function" ? options : callback;
    if (typeof cb !== "function") throw new TypeError('The "cb" argument must be of type function');
    let out;
    try {
      out = walk(p, typeof options === "function" ? undefined : options);
    } catch (e) {
      process.nextTick(cb, e);
      return;
    }
    process.nextTick(cb, null, out);
  };
  async_.native = async_;
  fs.realpath = async_;
  if (fs.promises) fs.promises.realpath = async (p, options) => walk(p, options);

  installBindingSeam(ancestorOfARoot, stripLongPrefix, refused, walk);
}

// The same repair one layer down, at the `fs` binding, which is what reaches ES modules.
//
// WHY THE LAYER ABOVE CANNOT. `internal/modules/esm/resolve.js` binds `const { realpathSync } =
// require('fs')` at module-load time, before any `--import` preload exists, so replacing the
// property leaves ESM calling the original; and on 25.7-26.0 the resolver is baked into the V8
// startup snapshot, where not even `--require` reaches it. `lib/fs.js` looks `binding.lstat` up
// on the binding object AT CALL TIME (~3273/3315/3357), so the destructured and snapshotted
// copies of `realpathSync` both observe a patch made here.
//
// WHAT THE SEAM IS ACTUALLY LOAD-BEARING FOR, measured layer-by-layer against a fs-layer-only
// stamp rather than assumed. A bare `import` under the simulated refusal dies with the fs layer
// alone on 18.19, 20.19, 22.15, 24.14, 25.9 and 26.0 and survives with this seam; on 24.17, 26.1
// and 26.5 the fs layer already suffices, because nodejs/node#62835 restored the resolver's late
// property read there. So this carries the whole 18.19-24.16 / 25.x / 26.0 band — which includes
// the 22 LTS line, where the fix was never backported. It ALSO carries, on every version, any
// caller holding a binding captured before the preload ran: a builtin's ESM named exports are
// snapshotted at first instantiation, so `import { realpathSync } from "node:fs"` is the
// pre-patch function and reaches `binding.lstat` directly.
//
// ADDITIVE, NEVER A REPLACEMENT. `fs.realpathSync.native` and `fs.promises.realpath` reach
// `binding.realpath`, a different native function the `lstat` patch does not touch, and the
// fs-layer substitutions above are what serve them — deleting those in favour of this seam is
// measured to break them (`esm_binding_seam_semantics`). An earlier reading said the two were
// interchangeable; its simulation refused only `binding.lstat`, i.e. it modelled less than the OS
// refuses, and a simulation that under-models produces a confident false pass.
function installBindingSeam(ancestorOfARoot, stripLongPrefix, refused, walk) {
  // GUARDED rather than assumed. `process.binding` is deprecated (DEP0111) and its removal must
  // not abort every confined child at startup — without it the fs-layer repair still stands and
  // only ESM regresses to failing loudly, which is the behaviour that ships today.
  let binding = null;
  try {
    binding = process.binding("fs");
  } catch {
    return;
  }
  if (!binding || typeof binding.lstat !== "function") return;

  // The realpath walk reaches `binding.lstat` in exactly two shapes, and `fs.lstatSync` reaches
  // the SAME function with the caller's own arguments — so the tolerance is scoped to those two
  // or it silently changes `fs.lstatSync` semantics for every path:
  //   `(base, true,  undefined, …)` — once per path component
  //   `(root, false, undefined, …)` — the Windows-only volume-root probe
  // The second one is `lstat 'C:\'`, i.e. the defect's primary Windows spelling, so scoping on
  // `bigint === true` alone would leave the headline case refused while every macOS simulation
  // stayed green (the root probe is guarded by `if (isWindows)`). The third argument separates
  // sync from the async form (an `FSReqCallback`) and the promises form (`kUsePromises`, a
  // symbol); a realpath walk is only ever the sync one.
  const isFsRoot = (p) => {
    const s = stripLongPrefix(path.resolve(String(p)));
    return path.parse(s).root === s;
  };
  const onRealpathWalk = (p, bigint, req) =>
    req === undefined && (bigint === true || (bigint === false && isFsRoot(p)));

  // A zeroed stats ARRAY, not a Stats instance: `lib/fs.js` reads the binding's result as a raw
  // indexable, and mode 0 is not S_IFLNK, so the walk treats the component as a plain directory
  // and keeps going — the same assertion `TOLERATED` makes one layer up. Returning `undefined`
  // instead would be read as "no entry" and make `realpathSync` return `undefined` SILENTLY.
  const fakeStats = (bigint) => (bigint ? new BigUint64Array(36) : new Float64Array(36));

  // The ctx convention carries a raw libuv `errno` and NO `code`, so `refused` — which reads
  // `e.code` — cannot be used there. The numbers are platform-specific (`UV_EPERM` is -4048 on
  // Windows, -1 on POSIX), hence the lookup rather than a comparison against literals.
  const refusedErrno = (errno) => {
    try {
      const name = getSystemErrorName(errno);
      return name === "EPERM" || name === "EACCES";
    } catch {
      return false;
    }
  };

  const origLstat = binding.lstat;
  binding.lstat = function (p, bigint, req, ctxOrThrow) {
    // Node 18 passes a ctx object in this position and returns `undefined` on failure; 20+
    // passes `throwIfNoEntry`, a boolean, and throws. Handling one convention and not the other
    // leaves that half of the version range silently unrepaired.
    if (ctxOrThrow !== null && typeof ctxOrThrow === "object") {
      const out = origLstat.call(this, p, bigint, req, ctxOrThrow);
      if (
        ctxOrThrow.errno !== undefined &&
        refusedErrno(ctxOrThrow.errno) &&
        onRealpathWalk(p, bigint, req) &&
        ancestorOfARoot(p)
      ) {
        delete ctxOrThrow.errno;
        delete ctxOrThrow.error;
        delete ctxOrThrow.syscall;
        return fakeStats(bigint);
      }
      return out;
    }
    try {
      return origLstat.call(this, p, bigint, req, ctxOrThrow);
    } catch (e) {
      if (refused(e) && onRealpathWalk(p, bigint, req) && ancestorOfARoot(p)) {
        return fakeStats(bigint);
      }
      throw e;
    }
  };

  if (typeof binding.realpath !== "function") return;
  // The other half of the seam. `fs.realpathSync.native` and `fs.promises.realpath` reach
  // `binding.realpath` — refused outright by the jail (`GetFinalPathNameByHandleW`) and untouched
  // by the `lstat` patch. Replacing the fs properties covers a caller that reads them at call
  // time; this covers one that captured them earlier, which is how a builtin ESM named import
  // (`import { realpath } from "node:fs/promises"`) reaches it, since those bindings are
  // snapshotted when the builtin's ESM facade is first instantiated. Delegating to the JS walk
  // loses the OS's own canonical spelling, the same trade the `.native` substitution above makes.
  binding.realpath = function (p, encoding, reqOrPromises, ctx) {
    if (typeof reqOrPromises === "symbol") {
      try {
        return Promise.resolve(walk(p, encoding));
      } catch (e) {
        return Promise.reject(e);
      }
    }
    if (reqOrPromises !== null && typeof reqOrPromises === "object") {
      let out;
      let err = null;
      try {
        out = walk(p, encoding);
      } catch (e) {
        err = e;
      }
      process.nextTick(() => reqOrPromises.oncomplete(err, out));
      return undefined;
    }
    if (ctx !== null && typeof ctx === "object") {
      try {
        return walk(p, encoding);
      } catch (e) {
        ctx.errno = e.errno;
        ctx.syscall = "realpath";
        ctx.path = String(p);
        return undefined;
      }
    }
    return walk(p, encoding);
  };
}
