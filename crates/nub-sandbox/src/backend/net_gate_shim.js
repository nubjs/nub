// Build jail, network axis: a PER-PACKAGE egress gate enforced inside the confined Node.
//
// WHY THIS EXISTS. Coarse egress denial is free on Linux (a seccomp AF_INET ceiling) and on
// macOS (Seatbelt denies network outright). NO PROXY is involved on any platform — the jail's
// egress decision is a per-package boolean, so there is nothing to route and no host to
// inspect; see the `There is deliberately NO host filtering` note below, which this line used
// to contradict. On Windows there is no unprivileged OS
// lever: withholding an AppContainer's `internetClient` capability is the only one, and being
// an AppContainer is precisely what breaks filesystem reads (a fresh LowBox profile sid is in
// no DACL). WFP is admin-gated. So on Windows the gate is USERLAND.
//
// WHAT IT IS AND IS NOT. This is NOT a security boundary. A native addon opening a raw socket
// bypasses it, and a non-Node child process is never reached by it. What it buys is the shape
// the threat model actually has: a worm that publishes a NEW lifecycle hook into a package that
// never had one, and phones home with plain `https.get` / `fetch` / `axios`. Blocking that
// forces a worm to ship a per-platform native socket addon to spread, which is a dramatically
// smaller blast radius. Measured against nub's 344-package corpus: 178 of the 179 packages that
// contact any host enter through Node or an npm `.cmd` bin shim, so the preload reaches 99.4%
// of the real surface; the one exception is a POSIX `.sh` that does not run on Windows.
//
// THE GATE IS A PER-PACKAGE BOOLEAN, resolved by `build_jail_net_allowed_for`: a catalog entry's own
// `network` value, or the BASELINE's when the package has no entry. It is NOT "catalogued ⇒ allowed" —
// possibly all of it, and it is not intercepted at all. A package with NO entry gets none. That
// is the whole defense: when an attacker bolts a postinstall onto `chalk`, `chalk` has no entry,
// so the hook cannot reach the network regardless of which host it wants.
//
// There is deliberately NO host filtering. It is the fragile half — a redirect is a second
// connection to a second host, so an allowlist naming only the origin denies the download at
// the second hop, and an upstream moving its CDN breaks a package the list claimed to permit —
// and it buys little, since narrowing hosts constrains a package somebody already reviewed and
// does nothing about the unvetted one, which is the actual threat.
//
// THE SEAM. Every TCP egress path in Node bottoms out in `net.Socket.prototype.connect` —
// measured: `http.get`, `http.request`, `fetch` (undici) and `net.connect` each call it exactly
// once, so `axios`/`node-fetch`/`got`/`ws`/`tls`/`http2` are covered transitively by the one
// patch rather than by chasing every high-level client. DNS and UDP are patched alongside it:
// not for TCP coverage, which `connect` already has, but because a query NAME and a datagram
// are each an exfil channel in their own right.
//
// DENIAL SHAPE IS DELIBERATE. A refused connection is delivered as an `error` EVENT carrying
// `ECONNREFUSED`-shaped fields, not as a synchronous throw, because that is what every HTTP
// client's existing error handling already expects — a throw from inside `connect` surfaces as
// an unhandled exception instead of a rejected request. Denied DNS reports `ENOTFOUND` for the
// same reason. The nub-specific reason rides along in `err.nubReason` for diagnostics without
// changing the shape a package matches on.
//
// DELIVERY. Static `import` (not `require`) because this is preloaded as a `data:text/javascript`
// ESM module via `--import`, the same channel as `windows_stdio_shim.js`: `defaultResolve`
// short-circuits on `data:` before touching the filesystem, so the preload needs no grant and
// the package it confines cannot tamper with it on disk.
//
// ⛔ BUT A MODULE THIS FILE ASSIGNS A TOP-LEVEL EXPORT ONTO IS ACQUIRED BY `require`, NOT
// `import`. A builtin's ESM NAMED exports are a SNAPSHOT: `BuiltinModule.syncExports()` copies
// `module.exports` onto the synthetic namespace once, when the ESM facade is first created
// (lib/internal/bootstrap/realm.js). So `import dns from "node:dns"` builds that facade from the
// ORIGINAL functions, and the later `dns.lookup = ...` below lands where the named exports can no
// longer see it — `import { lookup } from "node:dns"` in any package would bypass the gate
// entirely, as would `import { spawnSync } from "node:child_process"`.
//
// THE SHAPE DECIDES, NOT THE MODULE. A PROTOTYPE mutation (`net.Socket.prototype.connect`,
// `dgram.Socket.prototype.send`, `cp.ChildProcess.prototype.spawn`) mutates an object the named
// export already points AT, so it reaches ESM callers either way and those two stay plain
// imports. Only a top-level assignment needs `require`. MEASURED on Node 26 with a preload of
// each shape: `import` => top-level assignments ORIGINAL, prototype mutations PATCHED;
// `createRequire` => all PATCHED.
//
// AND IT IS WHAT KEEPS THE TWO PRELOADS ORDER-INDEPENDENT. This file and `windows_stdio_shim.js`
// both patch `cp.spawnSync` and ride ONE `NODE_OPTIONS` as two `--import` terms. While this file
// used a static `import`, whichever term ran FIRST decided whether the other's repair survived:
// measured, the stdio shim's fix reads PATCHED with the terms in today's order and ORIGINAL with
// them swapped — silently, with no error and no failing test. Acquiring by `require` here leaves
// the facade uncreated, so neither order can defeat the other.
import { createRequire } from "node:module";

const require_ = createRequire(process.execPath);
const dns = require_("node:dns");
const cp = require_("node:child_process");

import net from "node:net";
import dgram from "node:dgram";

// Captured at MODULE EVALUATION, which happens before any package code runs. This is what makes
// the child-env repair below un-defeatable by the obvious move: a script that does
// `delete process.env.NODE_OPTIONS` before spawning cannot erase a value already closed over.
const OWN_NODE_OPTIONS = process.env.NODE_OPTIONS;

// Replaced with a JSON literal by the Rust generator. Inlined rather than read from the
// environment on purpose: a confined script can rewrite `process.env`, but it cannot reach
// inside an already-evaluated `data:` module.
const POLICY = __NUB_NET_POLICY_JSON__;

const SENTINEL = "__nubJailNetGate";
if (!globalThis[SENTINEL]) {
  globalThis[SENTINEL] = true;
  install();
}

function install() {
  // A catalog-listed package gets NO patches at all — the permitted path stays byte-identical
  // to unjailed Node, which is strictly better than installing permissive interceptors that
  // still have to be reasoned about. This early return is the `allow` half of the boolean.
  if (POLICY.allow === true) return;

  // ── the predicate ────────────────────────────────────────────────────────────────────
  //
  // Under a deny the ONLY thing still permitted is what cannot carry data off the box.
  // Loopback qualifies, and exempting it is deliberate rather than a concession: a build that
  // starts a local server (dev-server probes, test harnesses, IPC over TCP) is common enough
  // that denying it buys nothing and breaks packages. Packages breaking is the cost this whole
  // design is optimised against; a loopback residual is not. This is NOT host permissioning —
  // there is no list, no wildcard and nothing per-package about it.
  const LOOPBACK = /^(127\.\d+\.\d+\.\d+|::1|0:0:0:0:0:0:0:1|localhost|.*\.localhost)$/i;

  // Named for what it now decides: whether a destination is NOT egress. Nothing here consults a
  // list, so there is no per-host policy to get wrong.
  function exempt(host) {
    if (host === undefined || host === null || host === "") return true; // no host: not egress
    return LOOPBACK.test(String(host).replace(/^\[|\]$/g, ""));
  }

  // Windows and POSIX report different errno integers for the same condition; a package that
  // reads `errno` rather than `code` must not be handed a value its own platform never emits.
  const ECONNREFUSED_ERRNO = process.platform === "win32" ? -4078 : -61;
  const ENOTFOUND_ERRNO = process.platform === "win32" ? -4058 : -3008;

  const pkg = POLICY.package || "this package";

  // ⛔ REPORT EVERY REFUSED DESTINATION, BECAUSE THE ERROR ALONE IS ROUTINELY SWALLOWED.
  //
  // The denial below is delivered as an `error` EVENT on the socket, which is the right shape for
  // a client's own error handling — and precisely why it so often vanishes: a downloader that
  // catches and falls back, or a `|| echo` in the lifecycle command, turns a refused connection
  // into a silent no-op. The capability search then sees a package that "fails at every rung" with
  // no reason attached, which is the single largest source of unexplained whole-disk grants.
  //
  // This line is the reason, printed once per distinct destination. It carries the HOST, which the
  // pass/fail ladder structurally cannot produce: the ladder learns THAT a package needs the
  // network, never WHERE it wanted to go.
  //
  // ⛔ IT LOGS ON THE DENY PATH ONLY, AND THAT IS A SECURITY PROPERTY, NOT A LIMITATION. An
  // "observe mode" that suppressed the denial would be an env-controlled egress bypass — the same
  // class of defect as making the trust list overridable by a flag. There is deliberately no knob
  // here: the decision is unchanged and unchangeable, and only the reporting is new.
  const reported = new Set();
  function report(host, api) {
    const target = host ? String(host) : "<unknown host>";
    if (reported.has(target)) return;
    reported.add(target);
    // stderr, not stdout: a lifecycle script's stdout is sometimes parsed by its own tooling.
    try {
      process.stderr.write(`WARN_NUB_JAIL_NET_DENIED ${pkg} ${target} ${api}\n`);
    } catch {
      // A closed stderr must never turn a network denial into a crash.
    }
  }

  // The remedy uses root-authored script approval, not dependency-controlled sandbox metadata.
  function denial(host, api) {
    report(host, api);
    const target = host ? String(host) : "<unknown host>";
    const err = new Error(
      `nub build sandbox: blocked network access to ${target} by ${pkg}. ${pkg}'s entry in ` +
        `nub's build catalog does not grant network access. If it genuinely needs it, that is a ` +
        `catalog PR. To run its install scripts unsandboxed, add to your package.json: ` +
        `"allowScripts": { "${POLICY.package || "<package>"}": "no-jail" }`,
    );
    err.code = "ECONNREFUSED";
    err.errno = ECONNREFUSED_ERRNO;
    err.syscall = "connect";
    err.nubReason = "ERR_NUB_JAIL_NET_DENIED";
    err.nubApi = api;
    err.nubHost = target;
    return err;
  }

  // ── TCP: the one seam that covers every client ───────────────────────────────────────
  const origConnect = net.Socket.prototype.connect;
  net.Socket.prototype.connect = function (...args) {
    // ⚠️ TWO CALL SHAPES, and missing the second one silently disables the whole gate.
    // `net.connect`, `http`, `https` and undici all run `normalizeArgs` themselves and then
    // call this with a SINGLE argument: the array `[options, cb]` (net's internal
    // `normalizedArgsSymbol` shape). Direct user code uses the documented `(options)` /
    // `(port[, host])` / `(path)` forms. An `Array.isArray` guard that EXCLUDES the array
    // therefore excludes every real client — measured: it let `http.get`, `fetch` and
    // `net.connect` straight through while `dns`/`dgram` still denied, which reads exactly
    // like "the TCP patch is not installed."
    const raw = args[0];
    const first = Array.isArray(raw) ? raw[0] : raw;
    let host;
    let isIpc = false;
    if (first !== null && typeof first === "object") {
      // A `path` option is a unix socket / Windows named pipe: IPC, not egress, and permitting
      // it is required — Node's own inspector and many build tools use one.
      if (first.path) isIpc = true;
      // Node itself defaults a host-less `connect(port)` to localhost; mirror that rather than
      // leaving `host` undefined, which the predicate would read as "not egress at all".
      else host = first.host ?? first.hostname ?? "localhost";
    } else if (typeof first === "string" && !/^\d+$/.test(first)) {
      isIpc = true; // connect(path[, cb])
    } else {
      host = typeof args[1] === "string" ? args[1] : "localhost"; // connect(port) → localhost
    }

    if (isIpc || exempt(host)) return origConnect.apply(this, args);

    // Delivered as an `error` event, never a throw — see the header note on denial shape. The
    // connection is never initiated, so this is a real deny and not a racing abort.
    process.nextTick(() => this.destroy(denial(host, "net.Socket.connect")));
    return this;
  };

  // ── DNS: an exfil channel in its own right (the query NAME carries the payload) ───────
  const denyLookup = (host, cb, api) => {
    const err = denial(host, api);
    err.code = "ENOTFOUND";
    err.errno = ENOTFOUND_ERRNO;
    err.syscall = "getaddrinfo";
    err.hostname = String(host);
    process.nextTick(() => cb(err));
  };

  const origLookup = dns.lookup;
  dns.lookup = function (hostname, options, callback) {
    const cb = typeof options === "function" ? options : callback;
    if (exempt(hostname) || typeof cb !== "function") {
      return origLookup.apply(this, arguments);
    }
    return denyLookup(hostname, cb, "dns.lookup");
  };
  if (dns.promises) {
    const origP = dns.promises.lookup;
    dns.promises.lookup = function (hostname, ...rest) {
      if (exempt(hostname)) return origP.apply(this, [hostname, ...rest]);
      return Promise.reject(denial(hostname, "dns.promises.lookup"));
    };
  }
  for (const fn of ["resolve", "resolve4", "resolve6", "resolveAny", "resolveTxt",
                    "resolveCname", "resolveMx", "resolveNs", "resolveSrv", "resolvePtr",
                    "resolveNaptr", "resolveSoa"]) {
    const orig = dns[fn];
    if (typeof orig !== "function") continue;
    dns[fn] = function (hostname, ...rest) {
      const cb = rest.find((a) => typeof a === "function");
      if (exempt(hostname) || typeof cb !== "function") {
        return orig.apply(this, [hostname, ...rest]);
      }
      return denyLookup(hostname, cb, `dns.${fn}`);
    };
  }

  // ── UDP: a datagram needs no handshake, so `connect` never sees it ───────────────────
  const origSend = dgram.Socket.prototype.send;
  dgram.Socket.prototype.send = function (...args) {
    // send(msg[, offset, length][, port][, address][, cb]) — the address is the last string
    // argument that is not the message itself.
    const address = args.slice(1).filter((a) => typeof a === "string").pop();
    if (exempt(address)) return origSend.apply(this, args);
    const err = denial(address, "dgram.send");
    const cb = args.find((a) => typeof a === "function");
    if (cb) process.nextTick(() => cb(err));
    else process.nextTick(() => this.emit("error", err));
    return undefined;
  };
  const origDgramConnect = dgram.Socket.prototype.connect;
  dgram.Socket.prototype.connect = function (...args) {
    const address = typeof args[1] === "string" ? args[1] : "localhost";
    if (exempt(address)) return origDgramConnect.apply(this, args);
    process.nextTick(() => this.emit("error", denial(address, "dgram.connect")));
    return undefined;
  };

  // ── children: keep the gate attached across a spawn ───────────────────────────────────
  //
  // MEASURED, both arms, before this existed: a spawned `node` inherits `NODE_OPTIONS` and is
  // gated for free, but `delete process.env.NODE_OPTIONS` before the spawn produced a child that
  // reached the sink — a one-line bypass. Re-stamping the captured value closes it, so defeating
  // the gate now requires more than forgetting to inherit it.
  //
  // The proxy variables are for the case the preload can NEVER reach: a non-Node child. `curl`,
  // `wget` and `git` all honour `http_proxy`/`https_proxy`, and so do the Node packages built on
  // `proxy-from-env` (axios). Pointing them at a closed loopback port turns egress into a
  // connection failure. This is additive only — it is not a boundary, a static binary or a client
  // that ignores proxy env sails past it, and that residual is accepted. Unconditional here
  // because `install` returned early on `allow:true` — under a deny there is no permitted host
  // the blackhole could break.
  const forceEnv = (env) => {
    const out = { ...env };
    if (OWN_NODE_OPTIONS !== undefined) out.NODE_OPTIONS = OWN_NODE_OPTIONS;
    for (const k of ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "all_proxy"]) {
      out[k] = "http://127.0.0.1:1";
    }
    delete out.NO_PROXY;
    delete out.no_proxy;
    return out;
  };

  // `envPairs` is the `KEY=value` array the async path has already materialised by this point;
  // rewriting it is the only place that catches every async spawn regardless of entry point.
  const origCpSpawn = cp.ChildProcess.prototype.spawn;
  cp.ChildProcess.prototype.spawn = function (options) {
    if (Array.isArray(options?.envPairs)) {
      const kept = options.envPairs.filter(
        (p) => !/^(NODE_OPTIONS|https?_proxy|HTTPS?_PROXY|ALL_PROXY|all_proxy|NO_PROXY|no_proxy)=/.test(p),
      );
      const forced = forceEnv({});
      options.envPairs = kept.concat(Object.entries(forced).map(([k, v]) => `${k}=${v}`));
    }
    return origCpSpawn.call(this, options);
  };

  // The sync family bottoms out in a module-local `spawnSync` that never reaches the seam above,
  // so it needs its own repair. An absent `options.env` means "inherit `process.env`" — which the
  // script may already have edited — so it must be materialised rather than left alone.
  const origCpSpawnSync = cp.spawnSync;
  cp.spawnSync = function (file, argsOrOptions, maybeOptions) {
    let args = argsOrOptions;
    let opts = maybeOptions;
    if (!Array.isArray(argsOrOptions) && argsOrOptions !== null && typeof argsOrOptions === "object") {
      args = undefined;
      opts = argsOrOptions;
    }
    const patched = { ...(opts || {}) };
    patched.env = forceEnv(patched.env || process.env);
    return args === undefined
      ? origCpSpawnSync(file, patched)
      : origCpSpawnSync(file, args, patched);
  };
}
