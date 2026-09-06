// Windows build jail: give a confined Node a working `child_process` — streaming piped stdio AND
// an IPC channel — without touching libuv.
//
// THE DEFECT. libuv names every stdio/IPC pipe in the GLOBAL NPFS namespace
// (`uv__unique_pipe_name` -> `\\?\pipe\uv\%llu-%lu`, deps/uv/src/win/pipe.c:109), which a LowBox
// token is refused — every shape, in every grant configuration, and no capability the policy
// surface can reach changes it. `uv__pipe_server` reads `ERROR_ACCESS_DENIED` as a NAME COLLISION
// and retries with a fresh name forever (`if (err != ERROR_PIPE_BUSY && err != ERROR_ACCESS_DENIED)
// goto error;`), inside `uv_spawn`, before any timer arms — so a confined piped spawn does not
// fail, it SPINS. Measured: cpu_ms 14906 of 15059 wall. The AppContainer-PRIVATE namespace
// (`\\.\pipe\LOCAL\…`) IS creatable under the same jail (measured on Server 2025 and Win 11 arm64,
// CI runs 30516750145 / 30517115980); libuv simply never spells a name there.
//
// SO THE PIPE IS CREATED HERE, IN THAT NAMESPACE, AND THE CHILD IS HANDED AN ALREADY-CONNECTED END.
// A `net` server binds a `LOCAL\` name, this process connects to it, and that connected end goes
// into `options.stdio` as a raw fd — libuv's `UV_INHERIT_FD`, which duplicates a handle it never
// had to name (deps/uv/src/win/process-stdio.c:254). The parent reads the accepted end, so output
// arrives AS IT IS PRODUCED and `maxBuffer` applies again. An `'ipc'` slot is removed outright and
// the channel re-implemented over the same private namespace as newline-delimited JSON — the
// framing Node's own `json` mode uses, which is what `fork` defaults to.
//
// WHY THE CHILD'S END IS AN fd AND NOT A STREAM. `UV_INHERIT_STREAM` also works under the jail
// (measured, `spawn-wrap-local=OK streamed="wrapmarker" eof=yes`), but it reads
// `((uv_pipe_t*) stream)->handle`, and on Windows `uv_pipe_connect2` parks the HANDLE on the
// connect REQ — only `uv__process_pipe_connect_req` moves it onto the handle, a loop turn later.
// `ChildProcess.prototype.spawn` is SYNCHRONOUS, so a stream connected in the same tick still reads
// INVALID_HANDLE_VALUE there and the spawn fails `ERROR_NOT_SUPPORTED`. An fd sidesteps the whole
// question: Windows gets one from `fs.openSync`, whose `CreateFileW` connects inline, and POSIX
// from `net.connect`, where `connect(2)` on a listening unix socket completes inline. Only that one
// primitive is platform-split; everything downstream is shared, so the force seam measures it all.
//
// WHY NODE'S OWN IPC IS INEXPRESSIBLE. `setupChannel` (lib/internal/child_process.js) must be
// handed a CONNECTED `Pipe(PipeConstants.IPC)`. It is module-internal, the `Pipe(IPC)` it wants is
// a local of `ChildProcess.prototype.spawn`, and the one route for adopting an existing handle —
// `Pipe.prototype.open` — is refused for IPC because `uv_pipe_open` asserts the handle is
// OVERLAPPED and `fs__open` never sets `FILE_FLAG_OVERLAPPED`. A `NODE_CHANNEL_FD` handoff
// therefore has no userland expression, which is why the channel is reimplemented, not reused.
//
// WHAT IS STILL REFUSED, DELIBERATELY. Handle passing (`child.send(msg, socket)`) needs
// `uv_write2`/`SCM_RIGHTS`, which no userland stream carries, and `serialization: 'advanced'` rides
// libuv's IPC frames. Both throw `ERR_NUB_SANDBOX_NO_IPC` naming the per-package opt-out: a clear,
// typed error beats an unkillable spin, and beats silently misbehaving.
//
// `spawnSync` KEEPS THE FILE-BACKED PATH, AND MUST. `ParseStdioOption` has no `'wrap'` case
// (`UNREACHABLE()`), and a synchronously-blocked loop cannot drain a pipe, so the sync family still
// rewrites each `'pipe'` slot to a scratch FILE. `execSync`, `execFileSync` and `spawnSync` buffer
// to completion anyway, so for them that is exact. It does need a writable directory — which is
// why ONLY the sync family probes for one now. The async path needs no filesystem grant at all,
// which matters in a jail whose whole purpose is confining writes, and is why the file path
// survives here only as the fallback for a namespace that refuses.
//
// SEAMS. `execFile`/`exec` call child_process's MODULE-LOCAL `spawn`, so patching the exported one
// would miss them; the single shared seam underneath all of them is `ChildProcess.prototype.spawn`.
// ONE override resolves the IPC slot and the piped slots TOGETHER — deliberately, because two
// stacked overrides would have an install order, and the wrong order would silently restore the
// refusal this file exists to remove. The sync family bottoms out in a module-local `spawnSync`
// instead, so `execSync`/`execFileSync` are re-expressed on the patched export; their error shaping
// (`inheritStderr`, `checkExecSyncError`) is reproduced from lib/child_process.js.
// ⛔ `require`, NOT `import` — AND THIS LINE IS THE WHOLE REASON THE SHIM APPLIES TO ESM CALLERS.
// A builtin's ESM NAMED exports are a SNAPSHOT: `BuiltinModule.syncExports()` copies
// `module.exports` onto the synthetic namespace only when the ESM facade is first created
// (lib/internal/bootstrap/realm.js). Importing `node:child_process` HERE creates that facade with
// the ORIGINAL functions, and every later mutation below lands on `module.exports` where the named
// exports can no longer see it — so `import { spawnSync } from "node:child_process"` in any package
// silently bypasses EVERY repair in this file, including the pipe repair it exists for.
//
// Acquiring it through `require` leaves the facade uncreated, so the first ESM importer builds it
// from the ALREADY-PATCHED exports. MEASURED on Node 26 with a preload of each shape:
// `import` => esm-named ORIGINAL / default patched / require patched; `createRequire` => all three
// patched. `process.execPath` is used as the resolution base because a `data:` preload has no path
// of its own.
import { createRequire } from "node:module";

const cp = createRequire(process.execPath)("node:child_process");
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { PassThrough, Readable, Writable } from "node:stream";

const SENTINEL = "__nubJailStdioShim";
const CHANNEL_ENV = "__NUB_JAIL_IPC";
const FORCE_ENV = "__NUB_JAIL_STDIO_SHIM_FORCE";
const WIN = process.platform === "win32";
const FORCE = process.env[FORCE_ENV] || "";
// A compressed loader keeps its transport URL in the evaluated module's fragment.
// Descendants must inherit that loader, not append the expanded module to NODE_OPTIONS.
const LOADER_FRAGMENT = new URL(import.meta.url).hash;
const SELF_URL = LOADER_FRAGMENT.startsWith('#loader=')
  ? decodeURIComponent(LOADER_FRAGMENT.slice('#loader='.length))
  : import.meta.url;

// The opt-out is root-authored script approval, not dependency-controlled sandbox metadata.
const IPC_HELP =
  "To run this package's build scripts unsandboxed, add to your package.json: " +
  '"allowScripts": { "<package>": "no-jail" }';

const refusal = (what) => {
  const e = new Error(
    `${what} is not available inside nub's Windows build sandbox. ${IPC_HELP}`,
  );
  e.code = "ERR_NUB_SANDBOX_NO_IPC";
  return e;
};

let addrSeq = 0;

// The force seam is what lets `stdio_shim_semantics` run on EVERY platform. What it repairs is a
// Windows fact, but how faithfully it reproduces `child_process` — incremental delivery, the
// deferred `close`, `maxBuffer`, the channel's own semantics, error shaping — is not, and a
// Windows-only regression test would leave the part most likely to break unmeasured on the machines
// people develop on. Inert without the preload: setting it loads nothing.
const active = WIN || !!FORCE;
if (active && !globalThis[SENTINEL]) {
  globalThis[SENTINEL] = true;
  install();
  // Awaited for the reason Node's own `_forkChild` runs during bootstrap: a package whose first
  // line calls `process.send()` must not race the socket.
  const inherited = process.env[CHANNEL_ENV];
  if (inherited) await installChildEnd(inherited);
}

function install() {
  // Only the SYNC family needs a file now, so this probe no longer gates the whole shim: a jail
  // with no writable directory anywhere still gets working async stdio and a working channel.
  // Neither candidate can be assumed — the temp dir is preferred but is not the jail's write
  // anchor and is often ungranted, and the cwd is the anchor but a script may have moved off it.
  //
  // The seam forces the no-writable-directory answer, because the property worth pinning — that
  // the async path has no scratch precondition — cannot be staged with real permissions off
  // Windows, where the address IS a filesystem path.
  const scratchDir = process.env.__NUB_JAIL_NO_SCRATCH ? null : writableScratchDir();

  let seq = 0;
  const scratch = (tag) =>
    path.join(scratchDir, `.nub-jail-${process.pid}-${seq++}-${tag}.tmp`);

  const isPipe = (slot) =>
    slot === undefined ||
    slot === null ||
    slot === "pipe" ||
    slot === "overlapped";

  const asArray = (stdio) =>
    Array.isArray(stdio) ? stdio.slice() : [stdio, stdio, stdio];

  // ⛔ `stdio: "ignore"` ON fd 0-2 IS REFUSED UNDER THE JAIL, AND IT LOOKS LIKE A BROKEN PACKAGE.
  // libuv maps `UV_IGNORE` on a real stdio fd to `CreateFileW(L"NUL")`
  // (deps/uv/src/win/process-stdio.c:153), and a LowBox token is refused `\Device\Null` — so
  // `uv_spawn` fails EPERM before the child starts. The same fact is already relied on two ways
  // below: `openChannel` refuses an "ipc" slot under fd 3 for exactly this reason, and it parks the
  // removed channel at a slot ABOVE fd 2 where libuv opens no device at all.
  //
  // MEASURED 2026-09-02, jailed vs jail-off, varying ONE thing: every shape built from pipes or
  // "inherit" spawns fine, while "ignore" and ["ignore","pipe","pipe"] are EPERM in ~2 ms — for a
  // System32 image AND for nub own granted jail-bin node. So it is the SLOT, never the image, which
  // is what refutes the older reading that a confined child cannot run System32 programs.
  //
  // COST WHEN UNREPAIRED, and why it is invisible: `cpu-features` (12.2M weekly downloads) pulls in
  // `buildcheck`, whose `findVS()` shells out to `powershell.exe` and `reg.exe` with
  // `stdio: ["ignore","pipe","pipe"]` INSIDE `try{}catch{}`. The refusal is swallowed, no MSVC
  // install is found, and the user is told `Unable to detect compiler type` — a message naming
  // neither a sandbox nor a spawn.
  //
  // A SCRATCH FILE, NOT A GRANT: `\Device\Null` security belongs to the system and widening it
  // would need privilege the build jail must never require. fd 0 gets an empty file, which reads EOF
  // exactly as NUL does; fd 1/2 get files nothing ever reads back. Slots above fd 2 are left alone.
  const isIgnore = (slot) => slot === "ignore";
  const hasIgnoredStdioFd = (stdio) => stdio.slice(0, 3).some(isIgnore);

  function closeScratch(opened) {
    for (const { fd } of opened) {
      try {
        fs.closeSync(fd);
      } catch {}
    }
    for (const { file } of opened) {
      try {
        fs.unlinkSync(file);
      } catch {}
    }
  }

  /// Substitutes a scratch-file fd for each "ignore" slot on fd 0-2, so libuv emits
  /// `UV_INHERIT_FD` and never reaches `uv__create_nul_handle`. `null` when no scratch directory
  /// is writable, so the caller can raise a typed refusal instead of returning an EPERM.
  function repairIgnoredSlots(stdio) {
    if (scratchDir === null) return null;
    const opened = [];
    try {
      for (let i = 0; i < stdio.length && i < 3; i++) {
        if (!isIgnore(stdio[i])) continue;
        const file = scratch(`n${i}`);
        if (i === 0) fs.writeFileSync(file, "");
        const fd = fs.openSync(file, i === 0 ? "r" : "w");
        opened.push({ fd, file });
        stdio[i] = fd;
      }
    } catch {
      closeScratch(opened);
      return null;
    }
    return opened;
  }

  // ── the async seam ────────────────────────────────────────────────────────────────
  const origSpawn = cp.ChildProcess.prototype.spawn;
  cp.ChildProcess.prototype.spawn = function (options) {
    const stdio = asArray(options && options.stdio);
    const ipcAt = stdio.indexOf("ipc");
    const piped = stdio.some(isPipe);
    if (ipcAt === -1 && !piped && !hasIgnoredStdioFd(stdio)) {
      return origSpawn.call(this, options);
    }

    // FIRST, so a refusal here cannot strand an opened channel or slot.
    const ignored = hasIgnoredStdioFd(stdio) ? repairIgnoredSlots(stdio) : [];
    if (ignored === null) throw refusal("an \"ignore\" stdio slot below fd 3");

    // Resolved FIRST, and in this same override — see SEAMS. A SECOND 'ipc' slot is deliberately
    // left in place so Node still raises its own `ERR_IPC_ONE_PIPE`.
    const channel = ipcAt === -1 ? null : openChannel(options, stdio, ipcAt);
    const slots = piped ? (openStreamedSlots(stdio) ?? openScratchSlots(stdio)) : null;
    if (piped && slots === null) throw refusal("piped stdio");

    stampChildEnv(options, channel && channel.address);
    options.stdio = stdio;

    let err;
    try {
      err = origSpawn.call(this, options);
    } catch (thrown) {
      if (channel) channel.abort();
      if (slots) slots.abort();
      closeScratch(ignored);
      throw thrown;
    }
    // Safe the instant `uv_spawn` returns: the child holds its own duplicated handles, and nothing
    // here is ever read back.
    closeScratch(ignored);
    // A spawn that never started still gets its streams — real Node exposes them too — but no
    // close accounting, which is Node's `pid !== 0` condition and is what keeps `onexit`'s own
    // `maybeClose` sufficient on the `error` path.
    if (slots) slots.attach(this);
    if (channel) {
      if (err) channel.abort();
      else channel.attach(this);
    }
    return err;
  };

  /// A channel over the container-private namespace, replacing the `'ipc'` slot outright.
  function openChannel(options, stdio, at) {
    if (options.serialization === "advanced") {
      throw refusal("serialization: 'advanced'");
    }
    // An 'ipc' slot below fd 3 would need `UV_IGNORE` on a real stdio fd, which opens
    // `\Device\Null` — refused on Server images under the same jail. Nothing in the ecosystem does
    // it; refusing beats trading one blocker for another.
    if (at < 3) throw refusal("an 'ipc' stdio slot below fd 3");

    const address = newAddress("ipc");
    const server = net.createServer({ pauseOnConnect: false });
    server.unref();
    // Bind happens synchronously inside `listen()` — only the 'listening' EVENT is deferred — so
    // the address exists by the time the child is launched.
    server.listen(address);
    // `UV_IGNORE` above fd 2 is a no-op in libuv: the slot is left INVALID_HANDLE_VALUE and no
    // device is opened (deps/uv/src/win/process-stdio.c:210). So this removes the channel without
    // substituting anything, which is what keeps `uv_spawn` off `uv__pipe_server` entirely.
    stdio[at] = "ignore";

    // Closed the instant the one expected connection lands, and again on teardown: a
    // `disconnect()` before the child ever connects would otherwise leave a listening pipe behind
    // for the life of the process, and a jail run spawns hundreds of children.
    const close = () => {
      try {
        server.close();
      } catch {}
    };
    return {
      address,
      abort: close,
      attach(child) {
        attachChannel(
          child,
          (bind) =>
            server.once("connection", (socket) => {
              close();
              bind(socket);
            }),
          close,
        );
      },
    };
  }

  /// One connected pipe per piped slot. `null` when the private namespace is unusable, so the
  /// caller can fall back to the scratch-file rewrite rather than lose stdio altogether.
  function openStreamedSlots(stdio) {
    const opened = [];
    try {
      for (let i = 0; i < stdio.length; i++) {
        if (!isPipe(stdio[i])) continue;
        const address = newAddress("io");
        const server = net.createServer({ pauseOnConnect: true });
        server.unref();
        const accepted = new Promise((resolve, reject) => {
          server.once("connection", resolve);
          server.once("error", reject);
        });
        // The connection is OURS and is established before the child starts, so this settles on
        // the next loop turn whatever the child does — but it has to be marked handled now, or an
        // abort before `attach` surfaces as an unhandled rejection.
        accepted.catch(() => {});
        server.listen(address);
        const end = openChildEnd(address);
        opened.push({ slot: i, server, accepted, fd: end.fd, release: end.release });
        stdio[i] = end.fd;
      }
    } catch {
      for (const p of opened) {
        p.release();
        try {
          p.server.close();
        } catch {}
      }
      return null;
    }
    return streamedPlan(opened);
  }

  function streamedPlan(opened) {
    const closeServer = (p) => {
      try {
        p.server.close();
      } catch {}
    };
    return {
      abort() {
        for (const p of opened) {
          p.release();
          closeServer(p);
        }
      },
      attach(child) {
        const streams = [];
        for (const p of opened) {
          const stream = new PassThrough();
          streams[p.slot] = stream;
          // Node's own accounting, mirrored exactly: stdin is never counted, and a spawn that
          // never started counts nothing (lib/internal/child_process.js).
          const counted = p.slot > 0 && child.pid !== 0;
          if (counted) child._closesNeeded++;
          p.accepted.then(
            (socket) => {
              closeServer(p);
              if (p.slot === 0) stream.pipe(socket);
              else socket.pipe(stream);
              // In real Node the exposed stream IS the socket, so destroying it closes the pipe and
              // settles the close accounting. Here they are two objects, and keeping that identity
              // is load-bearing: `execFile`'s maxBuffer path calls `child.stdout.destroy()`, and if
              // that left the socket open the count would never complete and the CALLBACK WOULD
              // NEVER FIRE — silently, exit 0, which is what the file-backed path does today.
              stream.once("close", () => socket.destroy());
              // Accounting rides the SOCKET, not the PassThrough: real pipes emit `close` whether
              // or not the caller ever read them, and gating on a PassThrough nobody drains would
              // hang the child's own completion instead.
              if (counted) socket.once("close", () => bumpClose(child));
            },
            () => {
              closeServer(p);
              stream.end();
              if (counted) bumpClose(child);
            },
          );
        }
        // The parent's copy of each child-end handle must go, or EOF never arrives on the reading
        // end. The duplicate the child holds is independent (`uv__duplicate_fd`) and was made
        // synchronously inside `_handle.spawn`, so releasing now is safe — and a tick earlier than
        // waiting for 'spawn' would be.
        for (const p of opened) p.release();
        publishStreams(child, streams);
      },
    };
  }

  /// The pre-streaming path, behaviour unchanged: every piped slot becomes a scratch FILE and the
  /// bytes are handed back at child exit. Reachable now only when the private namespace refuses,
  /// and the only path `spawnSync` can use at all.
  function openScratchSlots(stdio) {
    if (scratchDir === null) return null;
    // A pipe past index 2 is an extra channel with no file analogue — refuse rather than silently
    // drop it. The streamed path above has no such limit.
    for (let i = 3; i < stdio.length; i++) {
      if (isPipe(stdio[i])) throw refusal("a piped stdio slot above fd 2");
    }
    const fds = [];
    const captures = [];
    let stdinPiped = false;
    try {
      for (let i = 0; i < stdio.length && i < 3; i++) {
        if (!isPipe(stdio[i])) continue;
        const file = scratch(`a${i}`);
        if (i === 0) {
          fs.writeFileSync(file, "");
          const fd = fs.openSync(file, "r");
          fds.push({ fd, file });
          stdio[0] = fd;
          stdinPiped = true;
        } else {
          const fd = fs.openSync(file, "w");
          fds.push({ fd, file });
          stdio[i] = fd;
          captures[i] = file;
        }
      }
    } catch {
      return null;
    }
    return scratchPlan(captures, fds, stdinPiped);
  }

  function scratchPlan(captures, fds, stdinPiped) {
    const discard = () => {
      for (const { fd } of fds) {
        try {
          fs.closeSync(fd);
        } catch {}
      }
      for (const { file } of fds) {
        try {
          fs.unlinkSync(file);
        } catch {}
      }
    };
    return {
      abort: discard,
      attach(child) {
        const streams = [];
        for (const i of [1, 2]) {
          if (captures[i] === undefined) continue;
          streams[i] = new Readable({ read() {} });
          if (child.pid !== 0) child._closesNeeded++;
        }
        if (stdinPiped) {
          // A write here cannot reach the child — its stdin was opened on an already-EOF file
          // before it started. Accepting and discarding keeps `child.stdin.end()` from throwing on
          // a null; a caller genuinely conversing over stdin is the residual the streamed path
          // above exists to remove.
          streams[0] = new Writable({
            write(_chunk, _enc, done) {
              done();
            },
          });
        }
        publishStreams(child, streams);

        // `error` as well as `exit`: a spawn that fails outright emits `error` INSTEAD of `exit`,
        // and a caller waiting on `close` would then wait forever.
        let flushed = false;
        const flush = () => {
          if (flushed) return;
          flushed = true;
          for (const { fd } of fds) {
            try {
              fs.closeSync(fd);
            } catch {}
          }
          for (const i of [1, 2]) {
            const stream = streams[i];
            if (!stream) continue;
            let buf;
            try {
              buf = fs.readFileSync(captures[i]);
            } catch {
              buf = Buffer.alloc(0);
            }
            if (child.pid !== 0) stream.on("end", () => bumpClose(child));
            if (buf.length > 0) stream.push(buf);
            stream.push(null);
            // `end` only fires on a stream someone is reading, and `close` is gated on `end`. A
            // caller that never reads must not be able to hang the child's own completion.
            if (!stream.readableFlowing) stream.resume();
          }
          for (const { file } of fds) {
            try {
              fs.unlinkSync(file);
            } catch {}
          }
        };
        child.once("exit", flush);
        child.once("error", flush);
      },
    };
  }

  // ── the sync seam ─────────────────────────────────────────────────────────────────
  const origSpawnSync = cp.spawnSync;
  cp.spawnSync = function (file, argsOrOptions, maybeOptions) {
    const [args, given] = splitArgs(argsOrOptions, maybeOptions);
    const stdio = asArray(given && given.stdio);
    if (stdio.includes("ipc")) throw refusal("a synchronous IPC channel");
    if (!stdio.some(isPipe) && !hasIgnoredStdioFd(stdio)) {
      return origSpawnSync(file, args, given);
    }
    // There is no streamed path to fall back to here, so without a writable directory there is no
    // repair to make — and a typed refusal beats returning the caller to the spin, or to an EPERM
    // that names nothing.
    if (scratchDir === null) throw refusal("synchronous piped or ignored stdio");

    for (let i = 3; i < stdio.length; i++) {
      if (isPipe(stdio[i])) throw refusal("a piped stdio slot above fd 2");
    }

    const options = Object.assign({}, given);
    const fds = [];
    const captures = [null, null, null];
    // NOT `?? []`: swallowing a failed repair would hand the caller straight back to the EPERM
    // this exists to remove, with nothing said about why.
    const ignored = repairIgnoredSlots(stdio);
    if (ignored === null) throw refusal("synchronous piped or ignored stdio");
    for (let i = 0; i < 3; i++) {
      if (!isPipe(stdio[i])) continue;
      const scratchFile = scratch(`s${i}`);
      captures[i] = scratchFile;
      if (i === 0) {
        fs.writeFileSync(scratchFile, options.input ?? "");
        delete options.input;
        stdio[0] = fs.openSync(scratchFile, "r");
      } else {
        stdio[i] = fs.openSync(scratchFile, "w");
      }
      fds.push(stdio[i]);
    }
    options.stdio = stdio;

    const result = origSpawnSync(file, args, options);
    for (const fd of fds) {
      try {
        fs.closeSync(fd);
      } catch {}
    }
    closeScratch(ignored);
    const encoding =
      options.encoding && options.encoding !== "buffer" ? options.encoding : null;
    const read = (slot) => {
      if (captures[slot] === null) return null;
      let buf;
      try {
        buf = fs.readFileSync(captures[slot]);
      } catch {
        buf = Buffer.alloc(0);
      }
      return encoding ? buf.toString(encoding) : buf;
    };
    result.stdout = read(1);
    result.stderr = read(2);
    result.output = [null, result.stdout, result.stderr];
    for (const scratchFile of captures) {
      if (scratchFile === null) continue;
      try {
        fs.unlinkSync(scratchFile);
      } catch {}
    }
    return result;
  };

  // Reproduced from lib/child_process.js: both entry points forward the child's stderr to
  // the parent's when the caller named no stdio of its own, and both throw a generic error
  // carrying the whole result on a non-zero status.
  const checkSyncError = (result, description) => {
    if (result.error) return Object.assign(result.error, result);
    if (result.status === 0) return null;
    let message = `Command failed: ${description}`;
    if (result.stderr && result.stderr.length > 0) {
      message += `\n${result.stderr.toString()}`;
    }
    return Object.assign(new Error(message), result);
  };

  cp.execFileSync = function (file, argsOrOptions, maybeOptions) {
    const [args, given] = splitArgs(argsOrOptions, maybeOptions);
    const inheritStderr = !(given && given.stdio);
    const result = cp.spawnSync(file, args, given);
    if (inheritStderr && result.stderr) process.stderr.write(result.stderr);
    const error = checkSyncError(
      result,
      [(given && given.argv0) || file, ...(args || [])].join(" "),
    );
    if (error) throw error;
    return result.stdout;
  };

  cp.execSync = function (command, given) {
    const inheritStderr = !(given && given.stdio);
    const options = Object.assign({}, given);
    options.shell = typeof options.shell === "string" ? options.shell : true;
    const result = cp.spawnSync(command, undefined, options);
    if (inheritStderr && result.stderr) process.stderr.write(result.stderr);
    const error = checkSyncError(result, command);
    if (error) throw error;
    return result.stdout;
  };
}

/// Windows: the AppContainer-private namespace, the whole point of this file. Elsewhere a unix
/// socket, so the fidelity half is regression-tested where people develop — the namespace is a
/// Windows fact, the stream and channel semantics are not.
function newAddress(kind) {
  const tag = `nub-${kind}-${process.pid}-${addrSeq++}-${Math.random().toString(36).slice(2, 8)}`;
  return WIN ? `\\\\.\\pipe\\LOCAL\\${tag}` : path.join(os.tmpdir(), `${tag}.sock`);
}

/// The child's end of an already-bound address, obtained SYNCHRONOUSLY — the header explains why
/// that is the binding constraint and why the two platforms reach it differently.
function openChildEnd(address) {
  if (WIN) {
    // `r+` is the mapping that yields a named-pipe CLIENT open: with no O_CREAT/O_TRUNC, libuv's
    // `fs__open` picks `OPEN_EXISTING` with `FILE_GENERIC_READ | FILE_GENERIC_WRITE`
    // (deps/uv/src/win/fs.c), and `toNamespacedPath` leaves a `\\.\` path alone so the name
    // survives Node's own path handling.
    const fd = fs.openSync(address, "r+");
    return {
      fd,
      release() {
        try {
          fs.closeSync(fd);
        } catch {}
      },
    };
  }
  const socket = net.connect(address);
  const handle = socket._handle;
  if (!handle || typeof handle.fd !== "number" || handle.fd < 0) {
    socket.destroy();
    throw new Error("the connected child end exposed no fd");
  }
  return { fd: handle.fd, release: () => socket.destroy() };
}

/// Node's `maybeClose`, which is module-internal. `_closesNeeded`/`_closesGot` are Node's own
/// bookkeeping, so driving them by hand is what stops `close` landing before the output it is
/// meant to have delivered.
function bumpClose(child) {
  child._closesGot++;
  if (child._closesGot === child._closesNeeded) {
    child.emit("close", child.exitCode, child.signalCode);
  }
}

/// Mirrors the tail of Node's own `ChildProcess.prototype.spawn`: the three named properties plus
/// the positional `stdio` array, leaving every slot Node already resolved untouched.
function publishStreams(child, streams) {
  const combined = Array.isArray(child.stdio) ? child.stdio.slice() : [];
  while (combined.length < Math.max(3, streams.length)) combined.push(null);
  for (let i = 0; i < streams.length; i++) {
    if (streams[i]) combined[i] = streams[i];
  }
  child.stdio = combined;
  child.stdin = combined[0] || null;
  child.stdout = combined[1] || null;
  child.stderr = combined[2] || null;
}

/// The channel address, and — when the caller rebuilt `env` or emptied `execArgv` — the preload
/// chain the child would otherwise lose.
///
/// PRESERVATION, NOT CONSTRUCTION. In production this shim is one `--import` term on the
/// `NODE_OPTIONS` nub stamps (`build_jail_node_options`), alongside the per-package egress gate,
/// and that env var is also how BOTH reach every descendant. So the PARENT'S OWN value is the
/// source and `import.meta.url` only the last resort: re-deriving from this module alone would
/// silently drop the gate from every grandchild — a working `fork` that disarms the network
/// confinement underneath it. When the inherited value already carries this module, the pairs are
/// left exactly as they were.
function stampChildEnv(options, address) {
  const envPairs = Array.isArray(options.envPairs)
    ? options.envPairs
    : (options.envPairs = []);
  const inherited = envPairs.filter((p) => p.startsWith("NODE_OPTIONS=")).pop();
  const wanted = childNodeOptions(inherited);
  for (let i = envPairs.length - 1; i >= 0; i--) {
    // A stale address is dropped even for a non-IPC spawn, so a grandchild cannot attach to its
    // parent's channel through an `env` object captured earlier.
    if (envPairs[i].startsWith(`${CHANNEL_ENV}=`)) envPairs.splice(i, 1);
    else if (wanted !== undefined && envPairs[i].startsWith("NODE_OPTIONS=")) {
      envPairs.splice(i, 1);
    }
  }
  if (address) envPairs.push(`${CHANNEL_ENV}=${address}`);
  if (wanted !== undefined) {
    envPairs.push(`NODE_OPTIONS=${wanted}`);
    // The force seam's half of the same preservation. On Windows the preload is live because the
    // child is on Windows, so this adds nothing; off Windows the sentinel is the ONLY thing that
    // makes it live, and a caller who rebuilt `env` dropped it — which would leave the test suite
    // measuring an inert preload while reporting on a repaired one.
    if (FORCE && !envPairs.some((p) => p.startsWith(`${FORCE_ENV}=`))) {
      envPairs.push(`${FORCE_ENV}=${FORCE}`);
    }
  }
}

/// `undefined` means "leave the inherited pairs exactly alone" — the common case, and the one where
/// doing nothing is strictly safest.
function childNodeOptions(inheritedPair) {
  const prior =
    inheritedPair === undefined ? "" : inheritedPair.slice("NODE_OPTIONS=".length);
  if (prior.includes(SELF_URL)) return undefined;
  const fromParent = process.env.NODE_OPTIONS || "";
  if (fromParent.includes(SELF_URL)) return `${prior} ${fromParent}`.trim();
  return `${prior} --import ${SELF_URL}`.trim();
}

// ── the channel, shared by both ends ─────────────────────────────────────────────────
// Both ends run the identical protocol, so `send`/`'message'`/`disconnect` are defined once and
// bound onto `process` in the child and onto the ChildProcess in the parent. `socket` may arrive
// later than the first `send`, which is why writes queue.
function attachChannel(emitter, getSocket, onTeardown) {
  let socket = null;
  let pending = [];
  let closed = false;
  let refs = 0;

  const control = {
    ref() {
      refs++;
      if (socket) socket.ref();
    },
    unref() {
      refs--;
      if (socket) socket.unref();
    },
  };

  const onLine = (line) => {
    if (line.length === 0) return;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      // A partial line at close is not a message; Node's parser drops it too.
      return;
    }
    // `cmd: 'NODE_…'` is Node's internal namespace (`isInternal`), never delivered as 'message'.
    // Nothing here emits one; the guard exists so a real Node peer on the other end cannot leak
    // internals into user code.
    if (
      message &&
      typeof message === "object" &&
      typeof message.cmd === "string" &&
      message.cmd.startsWith("NODE_")
    ) {
      emitter.emit("internalMessage", message);
      return;
    }
    emitter.emit("message", message);
  };

  // Deferred, because Node's is: `child.disconnect()` on the line after `fork()` must still reach a
  // 'disconnect' listener registered on the line after THAT. A synchronous emit loses it, and the
  // loss is silent.
  const teardown = () => {
    if (closed) return;
    closed = true;
    emitter.connected = false;
    emitter.channel = null;
    pending = [];
    if (onTeardown) onTeardown();
    process.nextTick(() => emitter.emit("disconnect"));
  };

  const bind = (s) => {
    socket = s;
    socket.setEncoding("utf8");
    if (refs < 0) socket.unref();
    let buf = "";
    socket.on("data", (chunk) => {
      buf += chunk;
      let nl;
      while ((nl = buf.indexOf("\n")) !== -1) {
        const line = buf.slice(0, nl);
        buf = buf.slice(nl + 1);
        onLine(line);
      }
    });
    socket.on("close", teardown);
    socket.on("error", teardown);
    for (const [line, cb] of pending) socket.write(line, cb);
    pending = [];
  };

  emitter.channel = control;
  emitter.connected = true;

  emitter.send = function send(message, sendHandle, options, callback) {
    if (typeof sendHandle === "function") {
      callback = sendHandle;
      sendHandle = undefined;
    } else if (typeof options === "function") {
      callback = options;
      options = undefined;
    }
    if (sendHandle != null) throw refusal("child_process handle passing");
    // Node reports a closed channel through the 'error' event (or the callback), NOT by throwing —
    // `target.send`, lib/internal/child_process.js. Throwing instead turns a recoverable send into
    // an exception at a call site written to expect neither.
    if (closed) {
      const ex = new Error("Channel closed");
      ex.code = "ERR_IPC_CHANNEL_CLOSED";
      if (typeof callback === "function") process.nextTick(callback, ex);
      else process.nextTick(() => emitter.emit("error", ex));
      return false;
    }
    if (message === undefined) {
      const e = new TypeError('The "message" argument must be specified');
      e.code = "ERR_MISSING_ARGS";
      throw e;
    }
    const t = typeof message;
    if (t !== "string" && t !== "object" && t !== "number" && t !== "boolean") {
      const e = new TypeError(
        'The "message" argument must be of type string, object, number, or boolean',
      );
      e.code = "ERR_INVALID_ARG_TYPE";
      throw e;
    }
    const line = JSON.stringify(message) + "\n";
    if (socket) socket.write(line, callback);
    else pending.push([line, callback]);
    return true;
  };

  emitter.disconnect = function disconnect() {
    if (closed) return;
    if (socket) {
      emitter.connected = false;
      socket.end();
    } else {
      teardown();
    }
  };

  getSocket(bind);
  return control;
}

/// The child half. The address is deleted from `process.env` exactly as Node deletes
/// `NODE_CHANNEL_FD`, so a grandchild does not attach to its parent's channel.
async function installChildEnd(address) {
  delete process.env[CHANNEL_ENV];
  const socket = await new Promise((resolve, reject) => {
    const s = net.connect(address);
    s.once("connect", () => resolve(s));
    s.once("error", reject);
  });
  const control = attachChannel(process, (bind) => bind(socket));
  // Node keeps the channel unref'd until something listens, so an IPC child with no 'message'
  // listener still exits on its own (lib/child_process.js).
  socket.unref();
  process.on("newListener", (name) => {
    if (name === "message" || name === "disconnect") control.ref();
  });
  process.on("removeListener", (name) => {
    if (name === "message" || name === "disconnect") control.unref();
  });
}

/// `(args, options)`, `(options)` and `(args)` all reach the same parameter slot.
function splitArgs(first, second) {
  if (Array.isArray(first)) return [first, second];
  if (first !== null && typeof first === "object") return [undefined, first];
  return [first, second];
}

function writableScratchDir() {
  const candidates = [];
  try {
    candidates.push(os.tmpdir());
  } catch {}
  try {
    candidates.push(process.cwd());
  } catch {}
  for (const dir of candidates) {
    const probe = path.join(dir, `.nub-jail-probe-${process.pid}`);
    try {
      fs.writeFileSync(probe, "");
      fs.unlinkSync(probe);
      return dir;
    } catch {}
  }
  return null;
}
