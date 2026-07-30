// Windows build jail: give a confined Node a working `child_process` with piped stdio.
//
// WHY THIS EXISTS. libuv creates a NAMED PIPE per piped stdio stream, and the global NPFS
// namespace (`\\.\pipe\…`) is closed to a LowBox token — every shape is refused, in every
// grant configuration, and no capability the policy surface can reach changes it. The
// AppContainer-private namespace (`\\.\pipe\LOCAL\…`) IS open, but libuv does not spell its
// names that way. `uv__pipe_server` treats the refusal as a NAME COLLISION and retries with a
// fresh name forever (`if (err != ERROR_PIPE_BUSY && err != ERROR_ACCESS_DENIED) goto error;`),
// inside `uv_spawn`, before any timer arms — so a confined piped spawn does not fail, it SPINS.
// Measured: cpu_ms 14906 of 15059 wall.
//
// WHAT IT DOES. `stdio: [0, fd, fd]` against ordinary FILES is permitted under the same jail,
// so every 'pipe' slot is rewritten to a scratch file and the bytes are handed back through
// stream objects the caller cannot tell apart for the buffered case. `exec`, `execFile`,
// `execSync`, `execFileSync` and `spawnSync` buffer to completion anyway, so for them this is
// exact. A raw `spawn()` whose caller streams incrementally gets all of its output at child
// exit instead of as it is produced — the one behavioural difference, and the reason this is
// scoped to the jail rather than shipped to every Node nub runs.
//
// WHAT IT CANNOT DO. An IPC channel is a duplex pipe; a file cannot emulate one. `fork()` and
// an explicit `'ipc'` slot therefore FAIL FAST here with a diagnostic naming the opt-out,
// which is strictly better than the unkillable spin they would otherwise become. `maxBuffer`
// also stops applying — a file has no backpressure — so a runaway child fills the disk rather
// than tripping `ERR_CHILD_PROCESS_STDIO_MAXBUFFER`.
//
// SEAMS. `execFile`/`exec` call child_process's MODULE-LOCAL `spawn`, so patching the exported
// one would miss them; the single shared seam underneath all of them is
// `ChildProcess.prototype.spawn`. The sync family bottoms out in a module-local `spawnSync`
// instead, so `execSync`/`execFileSync` are re-expressed on the patched export — their error
// shaping (`inheritStderr`, `checkExecSyncError`) is reproduced from lib/child_process.js.
import cp from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Readable, Writable } from "node:stream";

// The force seam is what lets `stdio_shim_semantics` run on EVERY platform. What it repairs is
// a Windows fact, but how faithfully it reproduces `child_process` — buffered output, the
// deferred `close`, error shaping — is not, and a Windows-only regression test would leave the
// part most likely to break unmeasured on the machines people develop on. Inert without the
// preload: setting it loads nothing.
const SENTINEL = "__nubJailStdioShim";
const active = process.platform === "win32" || !!process.env.__NUB_JAIL_STDIO_SHIM_FORCE;
if (active && !globalThis[SENTINEL]) {
  globalThis[SENTINEL] = true;
  install();
}

function install() {
  // Every scratch file must land somewhere the jail granted WRITE. Neither candidate can be
  // assumed — the temp dir is preferred but is not the jail's write anchor and is often
  // ungranted, and the cwd is the anchor but a script may have moved off it — so writability
  // is PROBED. With neither writable there is no repair to make, and leaving child_process
  // untouched beats half-patching it.
  const dir = writableScratchDir();
  if (dir === null) return;

  let seq = 0;
  const scratch = (tag) =>
    path.join(dir, `.nub-jail-${process.pid}-${seq++}-${tag}.tmp`);

  const IPC_HELP =
    "nub's Windows build sandbox cannot provide a child_process IPC channel: Windows " +
    "refuses named pipes to a sandboxed (AppContainer) process, and Node's IPC channel " +
    "requires one. To run this package's build scripts unsandboxed, add to your " +
    'package.json: "dependenciesMeta": { "<package>": { "sandbox": false } }';

  const ipcError = (api) => {
    const e = new Error(`${api}: ${IPC_HELP}`);
    e.code = "ERR_NUB_SANDBOX_NO_IPC";
    return e;
  };

  const isPipe = (slot) =>
    slot === undefined ||
    slot === null ||
    slot === "pipe" ||
    slot === "overlapped";

  const asArray = (stdio) =>
    Array.isArray(stdio) ? stdio.slice() : [stdio, stdio, stdio];

  // ── the async seam ────────────────────────────────────────────────────────────────
  const origSpawn = cp.ChildProcess.prototype.spawn;
  cp.ChildProcess.prototype.spawn = function (options) {
    const stdio = asArray(options && options.stdio);
    if (stdio.includes("ipc")) throw ipcError("spawn");
    if (!stdio.some(isPipe)) return origSpawn.call(this, options);

    const fds = [];
    const captures = [null, null, null];
    let stdinPiped = false;
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
    // A pipe past index 2 is an extra channel with no file analogue (it has no stream on
    // the child either) — refuse rather than silently drop it.
    for (let i = 3; i < stdio.length; i++) {
      if (isPipe(stdio[i])) throw ipcError("spawn");
    }
    options.stdio = stdio;

    const err = origSpawn.call(this, options);
    attach(this, captures, fds, stdinPiped);
    return err;
  };

  /// Replace the null streams an fd-stdio spawn produces with ones fed from the scratch
  /// files at child exit, and hold `close` back until they have drained. `_closesNeeded`
  /// is Node's own bookkeeping (`maybeClose`), so raising it is what stops the base
  /// implementation emitting `close` before a single byte has been delivered.
  function attach(child, captures, fds, stdinPiped) {
    const streams = [null, null, null];
    for (const i of [1, 2]) {
      if (captures[i] === null) continue;
      streams[i] = new Readable({ read() {} });
      child._closesNeeded++;
    }
    if (streams[1]) child.stdout = streams[1];
    if (streams[2]) child.stderr = streams[2];
    if (stdinPiped) {
      // A write here cannot reach the child — its stdin was opened on an already-EOF file
      // before it started. Accepting and discarding keeps `child.stdin.end()` from throwing
      // on a null; a caller genuinely conversing over stdin is the documented residual.
      child.stdin = new Writable({
        write(_chunk, _enc, done) {
          done();
        },
      });
    }
    child.stdio = [child.stdin || null, child.stdout || null, child.stderr || null];

    // `error` as well as `exit`: a spawn that fails outright (a negative exit code, e.g. the
    // image was not found) emits `error` INSTEAD of `exit`, and a caller waiting on `close`
    // would then wait forever — the failure mode this whole shim exists to remove.
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
        stream.on("end", () => {
          child._closesGot++;
          if (child._closesGot === child._closesNeeded) {
            child.emit("close", child.exitCode, child.signalCode);
          }
        });
        if (buf.length > 0) stream.push(buf);
        stream.push(null);
        // `end` only fires on a stream someone is reading, and `close` is gated on `end`.
        // A caller that never reads must not be able to hang the child's own completion.
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
  }

  // ── the sync seam ─────────────────────────────────────────────────────────────────
  const origSpawnSync = cp.spawnSync;
  cp.spawnSync = function (file, argsOrOptions, maybeOptions) {
    const [args, given] = splitArgs(argsOrOptions, maybeOptions);
    const stdio = asArray(given && given.stdio);
    if (stdio.includes("ipc")) throw ipcError("spawnSync");
    if (!stdio.some(isPipe)) return origSpawnSync(file, args, given);

    for (let i = 3; i < stdio.length; i++) {
      if (isPipe(stdio[i])) throw ipcError("spawnSync");
    }

    const options = Object.assign({}, given);
    const fds = [];
    const captures = [null, null, null];
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

  // ── what has no file analogue ─────────────────────────────────────────────────────
  cp.fork = function () {
    throw ipcError("fork");
  };
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
