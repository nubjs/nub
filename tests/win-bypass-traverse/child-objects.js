// The OBJECT-level operations table: the NUL device, the two named-pipe namespaces, and the four
// `child_process` stdio shapes. Runs INSIDE the launched process, exactly like `child.js`, and is
// equally dumb about expectations — every verdict is asserted by `probe.ps1`, which knows the arm.
//
// WHY A SECOND CHILD RATHER THAN MORE CELLS IN `child.js`. `child.js` and its verdict are
// validated against six synthetic worlds and answer a filesystem question. These cells answer a
// different one and include an op that HANGS FOREVER under the jail, so they get their own arms
// with their own launch timeout. That keeps the fs table's arms — and their controls — untouched.
//
// ORDERING IS LOAD-BEARING. `child:objects-done` is emitted BEFORE the piped spawn, because that
// spawn does not fail, it spins: libuv's `uv__pipe_server` treats the namespace denial as a name
// collision and retries forever (`deps/uv/src/win/pipe.c`), inside `uv_spawn`, before any timer
// arms. Anything printed after it in a hanging arm is lost, so the marker that says "the table
// completed" must precede the op that can take the process down.

'use strict';
const fs = require('fs');
const net = require('net');
const cp = require('child_process');

const ARM = process.env.BT_ARM || '(unset)';
const NODE = process.execPath;
const nonce = Math.random().toString(36).slice(2, 10);

function one(line) {
  process.stdout.write(String(line).replace(/[\r\n]+/g, ' ') + '\n');
}
function ok(name, detail) {
  one('op:' + name + '=OK ' + (detail === undefined ? '' : String(detail)));
}
function err(name, e) {
  const code = (e && (e.code || e.errno)) || 'throw';
  one('op:' + name + '=ERR ' + code + ' ' + String((e && e.message) || e).slice(0, 180));
}
function run(name, fn) {
  try {
    ok(name, fn());
  } catch (e) {
    err(name, e);
  }
}

one('child:start arm=' + ARM + ' node=' + process.versions.node + ' exec=' + NODE);

// ── THE NUL DEVICE ────────────────────────────────────────────────────────────────────────
// `\Device\Null`'s security descriptor is reset by the kernel at every boot and the default does
// not name the AppContainer sids, so a LowBox child is refused it — measured independently by
// Microsoft's own mxc (`docs/host-prep.md`, `prepare-null-device`) and by nub's sibling probe,
// where `Command::output()`'s `Stdio::null()` failed `Access is denied` and read as an EXECUTE
// denial on the interpreter it was about to spawn (`crates/nub-core/src/node/discovery.rs`).
//
// Opened for read and for write separately: they are distinct access masks against one descriptor,
// and `uv__create_nul_handle` asks for FILE_GENERIC_READ on fd 0 and FILE_GENERIC_WRITE on 1 and 2.
run('nul-open-read', () => {
  const fd = fs.openSync('\\\\.\\NUL', 'r');
  fs.closeSync(fd);
  return 'opened';
});
run('nul-open-write', () => {
  const fd = fs.openSync('\\\\.\\NUL', 'w');
  fs.writeSync(fd, 'discard');
  fs.closeSync(fd);
  return 'opened+wrote';
});

// ── THE TWO NAMED-PIPE NAMESPACES, through libuv's own CreateNamedPipeW ────────────────────
// `uv_pipe_bind` calls `CreateNamedPipeW` once with the name given and returns its error, so this
// measures the namespace gate at the same call libuv's stdio path uses — but on the code path that
// REPORTS the failure instead of the one that retries it. `\\.\pipe\LOCAL\…` is the AppContainer's
// per-container namespace, which `CreateNamedPipeA`'s own reference says an app container must use.
function listenOnce(name, pipePath) {
  return new Promise((resolve) => {
    let settled = false;
    const done = (line) => {
      if (settled) return;
      settled = true;
      clearTimeout(guard);
      one('op:' + name + '=' + line);
      try {
        srv.close();
      } catch (e) {}
      resolve();
    };
    const guard = setTimeout(() => done('ERR HUNG no listen event within 6s'), 6000);
    const srv = net.createServer();
    srv.on('error', (e) => done('ERR ' + (e.code || e.errno || 'throw') + ' ' + String(e.message).slice(0, 120)));
    srv.listen(pipePath, () => done('OK bound ' + pipePath));
  });
}

// ── THE FOUR STDIO SHAPES ─────────────────────────────────────────────────────────────────
// `inherit` hands the child the parent's already-open handles, so it touches neither device — the
// liveness control that separates "this arm cannot spawn at all" from "this arm cannot spawn THIS
// WAY". `ignore` is the NUL path: libuv opens `NUL` fresh in the spawning process for fds 0-2.
// `fdfile` is the measured mitigation (file-backed descriptors in place of pipes). `pipe` is the
// blocker, and it is last.
// RUN 30512950258 BUG, caught by the unconfined control exactly as it is designed to be: with
// `stdio: 'inherit'` the grandchild writes into the SAME log handle this table is written to, and an
// UNTERMINATED `pong` concatenated with the next `op:` line so it no longer matched `^op:`. The cell
// then read MISSING-OP in every arm, unconfined included — a harness artifact that would have looked
// like "the confined child cannot spawn at all". The grandchild's output is now newline-delimited on
// both sides, so it occupies its own line and the marker survives.
const GRANDCHILD = 'process.stdout.write("\\ndiag:grandchild-ran\\n")';

function spawnCell(name, opts) {
  run(name, () => {
    const r = cp.spawnSync(NODE, ['-e', GRANDCHILD], opts);
    if (r.error) throw r.error;
    return 'status=' + r.status + ' out=' + String((r.stdout && r.stdout.toString()) || '').trim().slice(0, 40);
  });
}

// `BT_OBJ_MODE=fork` runs ONE cell and nothing else. `child_process.fork` opens an IPC channel,
// which is a `uv_pipe` with `ipc=1` — the same `uv__create_pipe_pair` -> `uv__pipe_server` path, in
// the same global namespace. So the file-descriptor mitigation cannot reach it: there is no stdio
// option that removes an IPC channel from a fork. It needs its own arm because it is expected to
// hang, and a cell placed after another hanging cell never runs.
async function forkMode() {
  one('child:fork-arm-start arm=' + ARM);
  run('fork-ipc', () => {
    // The forkee is written into the granted data dir rather than reusing this file, so the cell
    // cannot recurse. It never runs if the hypothesis holds: the IPC pipe is created inside the
    // `fork` call itself, so the spin happens before any child code exists.
    const forkee = require('path').join(require('path').dirname(process.env.BT_OBJ_SINK), 'forkee.js');
    fs.writeFileSync(forkee, 'process.exit(0);\n');
    const child = cp.fork(forkee, [], { stdio: 'inherit' });
    try {
      child.kill();
    } catch (e) {}
    return 'forked pid=' + child.pid;
  });
  one('child:fork-returned arm=' + ARM);
}

async function main() {
  if (process.env.BT_OBJ_MODE === 'fork') return forkMode();

  await listenOnce('pipe-listen-global', '\\\\.\\pipe\\nubobj-g-' + nonce);
  await listenOnce('pipe-listen-local', '\\\\.\\pipe\\LOCAL\\nubobj-l-' + nonce);

  spawnCell('spawn-inherit', { stdio: 'inherit' });
  spawnCell('spawn-ignore', { stdio: 'ignore' });

  run('spawn-fdfile', () => {
    // The mitigation shape: a real file descriptor for stdout/stderr, no pipe and no NUL. stdin is
    // fd 0, which the launcher already opened and inherited in, so the child never opens it either.
    const sinkPath = process.env.BT_OBJ_SINK;
    const fd = fs.openSync(sinkPath, 'w');
    try {
      const r = cp.spawnSync(NODE, ['-e', GRANDCHILD], { stdio: [0, fd, fd] });
      if (r.error) throw r.error;
      fs.closeSync(fd);
      return 'status=' + r.status + ' sink=' + fs.readFileSync(sinkPath, 'utf8').trim().slice(0, 40);
    } catch (e) {
      try {
        fs.closeSync(fd);
      } catch (e2) {}
      throw e;
    }
  });

  one('child:objects-done arm=' + ARM);

  // THE BLOCKER. Not `ERR`, not slow — an unbounded busy retry inside `uv_spawn`, measured at
  // cpu≈wall. Node's own `timeout` option cannot break it because no timer has armed yet. The
  // launcher's wall clock is the only bound, so a missing `op:spawn-piped=` line IS the finding.
  spawnCell('spawn-piped', { encoding: 'utf8' });
  one('child:spawn-piped-returned arm=' + ARM);
}

main().then(
  () => process.exit(0),
  (e) => {
    one('child:fatal ' + String((e && e.stack) || e).replace(/[\r\n]+/g, ' '));
    process.exit(3);
  }
);
