// PROTOTYPE: give a confined Node a working `child_process` IPC channel without touching libuv.
//
// THE DEFECT. libuv names every stdio/IPC pipe in the GLOBAL NPFS namespace
// (`uv__unique_pipe_name` -> `\\?\pipe\uv\%llu-%lu`, deps/uv/src/win/pipe.c:109), which a LowBox
// token is refused. `uv__pipe_server` then reads `ERROR_ACCESS_DENIED` as a name collision and
// retries forever (`if (err != ERROR_PIPE_BUSY && err != ERROR_ACCESS_DENIED) goto error;
// random++;`) inside `uv_spawn`, before any timer arms — so a confined piped spawn does not fail,
// it pins a core. `\\.\pipe\LOCAL\…`, the AppContainer-private namespace, IS creatable under the
// same jail (measured, MECHANISM-FACTS §5i).
//
// WHY A USERLAND CHANNEL RATHER THAN NODE'S OWN. Node's IPC needs `setupChannel()`
// (lib/internal/child_process.js:619) to be handed a CONNECTED `Pipe(PipeConstants.IPC)`. Nothing
// reaches it: `setupChannel` is module-internal, and the only object a preload can touch is the
// `Pipe(IPC)` that `getValidStdio` creates behind `ChildProcess.prototype.spawn`, which is not
// visible at that seam. `Pipe.prototype.open(fd)` is the one connect-an-existing-handle route and
// `uv_pipe_open` rejects it for IPC: it asserts the handle is OVERLAPPED, and no userland API
// yields an overlapped pipe handle as a CRT fd (`fs__open` never sets `FILE_FLAG_OVERLAPPED`).
// So the channel is reimplemented on a plain duplex socket, newline-delimited JSON — byte-identical
// framing to Node's own `json` serialization mode
// (lib/internal/child_process/serialization.js), which is what `fork` defaults to.
//
// WHAT IS NOT REPRODUCED. Handle passing (`child.send(msg, socket)`) needs
// `uv_write2`/`SCM_RIGHTS`, which a userland stream cannot carry — it throws a diagnostic naming
// the opt-out instead of failing obscurely. `serialization: 'advanced'` is refused for the same
// reason (its framing rides libuv's IPC frames). Everything else — `send`, `'message'`,
// `'disconnect'`, `connected`, `channel.ref/unref`, `ERR_IPC_CHANNEL_CLOSED` after disconnect — is
// reproduced.
//
// The pipe namespace needs no filesystem grant, so unlike the file-backed stdio rewrite this
// route has no writable-scratch-dir precondition to probe and cannot bail for want of one.

import cp from "node:child_process";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const CHANNEL_ENV = "__NUB_JAIL_IPC";
const SENTINEL = "__nubJailIpcShim";

// Windows: the AppContainer-private namespace, the whole point of this file. Elsewhere a unix
// socket, which exists so the API fidelity above can be regression-tested on the machines people
// develop on — the namespace is a Windows fact, the channel semantics are not.
const WIN = process.platform === "win32";
let seq = 0;
function newChannelAddress() {
  const tag = `nub-ipc-${process.pid}-${seq++}-${Math.random().toString(36).slice(2, 8)}`;
  return WIN ? `\\\\.\\pipe\\LOCAL\\${tag}` : path.join(os.tmpdir(), `${tag}.sock`);
}

const ipcUnsupported = (what) => {
  const e = new Error(
    `${what} is not available inside nub's build sandbox: Windows refuses named pipes to an ` +
      "AppContainer process in the global namespace, and this channel is emulated over the " +
      'container-private one. To run this package unsandboxed: "dependenciesMeta": ' +
      '{ "<package>": { "sandbox": false } }',
  );
  e.code = "ERR_NUB_SANDBOX_NO_IPC";
  return e;
};

const closedError = () => {
  const e = new Error("Channel closed");
  e.code = "ERR_IPC_CHANNEL_CLOSED";
  return e;
};

// ── the shared channel half ──────────────────────────────────────────────────────────────────
// Both ends run the identical protocol, so `send`/`'message'`/`disconnect` are defined once and
// bound onto `process` in the child and onto the ChildProcess in the parent. `emitter` is what
// user code listens on; `socket` may arrive later than the first `send`, which is why writes queue.
function attachChannel(emitter, getSocket, onTeardown) {
  let socket = null;
  let pending = [];
  let closed = false;
  let refs = 0;

  const control = {
    ref() {
      refs++;
      socket?.ref();
    },
    unref() {
      refs--;
      socket?.unref();
    },
  };

  const flush = () => {
    for (const [line, cb] of pending) socket.write(line, cb);
    pending = [];
  };

  const onLine = (line) => {
    if (line.length === 0) return;
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      return; // A partial line at close is not a message; Node's parser drops it too.
    }
    // `cmd: 'NODE_…'` is Node's internal namespace (`isInternal`), never delivered as 'message'.
    // Nothing in this shim emits one; the guard exists so a real Node peer on the other end
    // (a mixed-mode child) cannot leak internals into user code.
    if (message && typeof message === "object" && typeof message.cmd === "string" && message.cmd.startsWith("NODE_")) {
      emitter.emit("internalMessage", message);
      return;
    }
    emitter.emit("message", message);
  };

  // Deferred, because Node's is: `child.disconnect()` on the line after `fork()` must still reach a
  // 'disconnect' listener registered on the line after THAT. A synchronous emit loses it, and the
  // loss is silent — the arm simply prints one line fewer than the unshimmed control.
  const teardown = () => {
    if (closed) return;
    closed = true;
    emitter.connected = false;
    emitter.channel = null;
    pending = [];
    onTeardown?.();
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
    flush();
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
    if (sendHandle != null) throw ipcUnsupported("child_process handle passing");
    // Node reports a closed channel through the 'error' event (or the callback), NOT by throwing —
    // `target.send`, lib/internal/child_process.js:777-783. Throwing instead turns a recoverable
    // send into an exception at a call site written to expect neither.
    if (closed) {
      const ex = closedError();
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
      const e = new TypeError(`The "message" argument must be of type string, object, number, or boolean`);
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

// ── the CHILD half ───────────────────────────────────────────────────────────────────────────
// Runs during `--import`, so `process.send` exists before the entry module is evaluated — the same
// ordering guarantee Node's own `_forkChild` gets from bootstrap. The connect is awaited for that
// reason: a package that calls `process.send()` on its first line must not race the socket.
// The address is deleted from `process.env` exactly as Node deletes `NODE_CHANNEL_FD`, so a
// grandchild does not attach to its parent's channel.
async function installChildEnd(address) {
  delete process.env[CHANNEL_ENV];
  const socket = await new Promise((resolve, reject) => {
    const s = net.connect(address);
    s.once("connect", () => resolve(s));
    s.once("error", reject);
  });
  const control = attachChannel(process, (bind) => bind(socket));
  // Node keeps the channel unref'd until something listens, so an IPC child with no 'message'
  // listener still exits on its own (lib/child_process.js:177-186).
  socket.unref();
  process.on("newListener", (name) => {
    if (name === "message" || name === "disconnect") control.ref();
  });
  process.on("removeListener", (name) => {
    if (name === "message" || name === "disconnect") control.unref();
  });
}

// ── the PARENT half ──────────────────────────────────────────────────────────────────────────
// `ChildProcess.prototype.spawn` is the single seam beneath `fork`/`spawn`/`execFile`/`exec`, and
// it sees `options.stdio` BEFORE `getValidStdio` turns it into the C++ form — which is what makes
// the 'ipc' slot removable. Removing it is what keeps `uv_spawn` off `uv__pipe_server` entirely:
// with no `UV_CREATE_PIPE` entry there is no `CreateNamedPipe` call to be refused.
function installParentEnd() {
  const origSpawn = cp.ChildProcess.prototype.spawn;

  cp.ChildProcess.prototype.spawn = function spawn(options) {
    const stdio = Array.isArray(options?.stdio)
      ? options.stdio.slice()
      : [options?.stdio, options?.stdio, options?.stdio];
    const at = stdio.indexOf("ipc");
    if (at === -1) return origSpawn.call(this, options);
    if (options.serialization === "advanced") throw ipcUnsupported("serialization: 'advanced'");
    // An 'ipc' slot below 3 would need `UV_IGNORE` on a real stdio fd, which opens `\Device\Null`
    // — refused on Server images under the same jail (§5i). Nothing in the ecosystem does it;
    // refusing beats trading one blocker for another.
    if (at < 3) throw ipcUnsupported("an 'ipc' stdio slot below fd 3");

    const address = newChannelAddress();
    const server = net.createServer({ pauseOnConnect: false });
    server.unref();
    // Bind is synchronous inside `listen()` (only the 'listening' EVENT is deferred), so the
    // address exists by the time the child is launched below.
    server.listen(address);

    // `UV_IGNORE` above fd 2 is a no-op in libuv — the slot is left INVALID_HANDLE_VALUE and no
    // device is opened (deps/uv/src/win/process-stdio.c:210) — so this removes the channel
    // without substituting anything.
    stdio[at] = "ignore";
    options.stdio = stdio;

    // The forkee needs the preload and the address. `execArgv` carries the preload for an
    // ordinary `fork` (it defaults to the parent's), but a caller that passes `execArgv: []` or
    // rebuilds `env` from scratch would strip it, so both are stamped explicitly.
    const envPairs = Array.isArray(options.envPairs) ? options.envPairs : (options.envPairs = []);
    const drop = new RegExp(`^(${CHANNEL_ENV}|NODE_OPTIONS)=`);
    const inherited = envPairs.filter((p) => /^NODE_OPTIONS=/.test(p)).pop();
    for (let i = envPairs.length - 1; i >= 0; i--) if (drop.test(envPairs[i])) envPairs.splice(i, 1);
    envPairs.push(`${CHANNEL_ENV}=${address}`);
    envPairs.push(`NODE_OPTIONS=${childNodeOptions(inherited)}`);

    const err = origSpawn.call(this, options);
    if (err) {
      server.close();
      return err;
    }

    // The server is closed the instant the one expected connection lands, and again on teardown —
    // a `disconnect()` before the child ever connects would otherwise leave a listening pipe (and,
    // off Windows, a socket file) behind for the life of the process. A build jail run spawns
    // hundreds of children, so a per-child leak is not cosmetic.
    attachChannel(
      this,
      (bind) => {
        server.once("connection", (socket) => {
          server.close();
          bind(socket);
        });
      },
      () => {
        try {
          server.close();
        } catch {}
      },
    );
    return err;
  };
}

// A caller that rebuilt `env` from scratch stripped NODE_OPTIONS, so one has to be synthesised. The
// PARENT'S OWN value is the right source and `import.meta.url` only the fallback: in production this
// file is one entry in a chain nub stamps (the stdio rewrite, the egress gate, the webstorage
// preload), and re-deriving from this module alone would silently drop the others from every
// descendant — a fix for `fork` that disarms the network gate underneath it.
function childNodeOptions(inheritedPair) {
  const prior = inheritedPair ? inheritedPair.slice("NODE_OPTIONS=".length) : "";
  if (prior.includes(import.meta.url)) return prior;
  const fromParent = process.env.NODE_OPTIONS || "";
  if (fromParent.includes(import.meta.url)) return `${prior} ${fromParent}`.trim();
  return `${prior} --import ${import.meta.url}`.trim();
}

if (!globalThis[SENTINEL]) {
  globalThis[SENTINEL] = true;
  installParentEnd();
  const address = process.env[CHANNEL_ENV];
  if (address) await installChildEnd(address);
}
