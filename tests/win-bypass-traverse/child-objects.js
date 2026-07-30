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

// `BT_OBJ_MODE=localpipe` — THE PHYSICS of the candidate fix, with no shim in the picture.
// `pipe-listen-local` already established that a LowBox process can CREATE a pipe in the
// container-private namespace. Two things it does not establish, and a repair needs both:
// whether the same process can CONNECT to that name, and whether a pipe end handed to a child
// through `stdio` reaches `uv_spawn` on a branch that does not call `CreateNamedPipe` at all.
// libuv's `UV_INHERIT_FD` and `UV_INHERIT_STREAM` branches only DUPLICATE an existing handle
// (`deps/uv/src/win/process-stdio.c:256,306`), so neither can be refused by the namespace — that
// is the prediction these cells test.
// The `LOCAL\` namespace is the Windows fact under test; the surrounding plumbing (a server, a
// connect, a Stream in the stdio array) is not. Off Windows these cells fall back to a unix socket so
// the two new arms can be smoke-tested on a dev machine before a CI run is spent on them — the
// AppContainer arms only ever run on a Windows runner, where this returns the real private name.
function localName(tag) {
  return process.platform === 'win32'
    ? '\\\\.\\pipe\\LOCAL\\nubobj-' + tag + '-' + nonce
    : require('path').join(require('os').tmpdir(), 'nubobj-' + tag + '-' + nonce + '.sock');
}

async function localPipeMode() {
  one('child:localpipe-arm-start arm=' + ARM);
  const pipePath = localName('lp');

  // A server whose one connection is served from the SAME process. Under the jail the peer will be
  // a child in the same AppContainer, i.e. the same private namespace, so a same-process connect is
  // the weaker of the two and the right thing to gate on.
  const srv = net.createServer();
  const connected = new Promise((resolve) => srv.once('connection', resolve));
  const listening = new Promise((resolve, reject) => {
    srv.once('error', reject);
    srv.listen(pipePath, resolve);
  });
  try {
    await listening;
  } catch (e) {
    err('pipe-connect-local', e);
    one('child:localpipe-done arm=' + ARM);
    return;
  }

  let client = null;
  await new Promise((resolve) => {
    const guard = setTimeout(() => {
      one('op:pipe-connect-local=ERR HUNG no connect within 6s');
      resolve();
    }, 6000);
    const c = net.connect(pipePath);
    c.once('error', (e) => {
      clearTimeout(guard);
      err('pipe-connect-local', e);
      resolve();
    });
    c.once('connect', async () => {
      clearTimeout(guard);
      client = c;
      // A full DUPLEX round trip, not just a connect: a handle that opens but cannot carry bytes in
      // both directions would read as a working channel and then fail somewhere with no evidence
      // attached. An emulated IPC channel needs both directions.
      const peer = await connected;
      peer.on('data', (d) => peer.write('echo:' + d.toString()));
      const back = new Promise((r) => c.once('data', (d) => r(d.toString())));
      c.write('rt\n');
      const got = await Promise.race([back, new Promise((r) => setTimeout(() => r('(timeout)'), 4000))]);
      ok('pipe-connect-local', 'duplex=' + JSON.stringify(got.trim()));
      resolve();
    });
  });

  if (client === null) {
    srv.close();
    one('child:localpipe-done arm=' + ARM);
    return;
  }
  client.destroy();

  // THE DECISIVE CELL. A second pipe, whose client end is handed to a child as its stdout via the
  // `stdio` array — `getValidStdio` turns a Stream into `{type:'wrap'}`
  // (lib/internal/child_process.js:1078) and libuv takes the `UV_INHERIT_STREAM` branch. If this
  // returns, an ordinary piped `spawn` is repairable in userland with real streaming semantics,
  // not just file-backed capture. If it spins, `UV_INHERIT_STREAM` is not the escape it looks like.
  const outPath = localName('lpout');
  const outSrv = net.createServer();
  const outConn = new Promise((resolve) => outSrv.once('connection', resolve));
  await new Promise((resolve, reject) => {
    outSrv.once('error', reject);
    outSrv.listen(outPath, resolve);
  });
  const childEnd = await new Promise((resolve, reject) => {
    const c = net.connect(outPath);
    c.once('connect', () => resolve(c));
    c.once('error', reject);
  });

  // Marker BEFORE the spawn: a spin inside `uv_spawn` takes the process down with no further
  // output, so the line that says "the table got this far" has to precede the risky op.
  one('child:localpipe-preswap arm=' + ARM);

  await new Promise((resolve) => {
    let done = false;
    const finish = (line) => {
      if (done) return;
      done = true;
      one('op:spawn-wrap-local=' + line);
      resolve();
    };
    let child;
    try {
      child = cp.spawn(NODE, ['-e', 'process.stdout.write("wrapmarker\\n")'], {
        stdio: ['inherit', childEnd, 'inherit'],
      });
    } catch (e) {
      finish('ERR ' + ((e && e.code) || 'throw') + ' ' + String((e && e.message) || e).slice(0, 140));
      return;
    }
    // The parent holds a duplicate of the handle the child got, so EOF on the reading end only
    // arrives once the parent drops its copy — which is also what production would have to do.
    child.once('spawn', () => childEnd.destroy());
    child.once('error', (e) => finish('ERR ' + ((e && e.code) || 'throw')));
    const guard = setTimeout(() => finish('ERR HUNG no bytes and no exit within 10s'), 10000);
    outConn.then((peer) => {
      let buf = '';
      peer.on('data', (d) => {
        buf += d.toString();
      });
      peer.on('end', () => {
        clearTimeout(guard);
        finish('OK streamed=' + JSON.stringify(buf.trim()) + ' eof=yes');
      });
    });
  });
  outSrv.close();
  srv.close();
  one('child:localpipe-done arm=' + ARM);
}

// `BT_OBJ_MODE=shimfork` — the CANDIDATE FIX, end to end. The arm is launched with
// `--import local-pipe-shim.mjs`, which replaces the 'ipc' stdio slot with a channel over a
// container-private pipe. Nothing here knows the shim exists: it calls plain `child_process.fork`
// and plain `process.send`, so a PASS means unmodified package code works, which is the only
// result worth having.
async function shimForkMode() {
  one('child:shimfork-arm-start arm=' + ARM);
  // An `op:` line rather than a note, so the verdict can REQUIRE it. Without this cell a preload
  // that failed to load is indistinguishable from one that loaded and did not help — the same
  // grant-never-landed shape the device arms already guard against, applied to a preload.
  one('op:shim-active=' + (globalThis.__nubJailIpcShim ? 'OK installed' : 'ERR not-installed'));
  const dir = require('path').dirname(process.env.BT_OBJ_SINK);

  const forkee = require('path').join(dir, 'shim-forkee.cjs');
  fs.writeFileSync(
    forkee,
    "process.on('message', (m) => { process.send({ pong: m.ping, connected: process.connected }); });\n",
  );

  // Nested, because the fix has to survive its own recursion: the grandchild is forked BY a forked
  // child, so the preload must reach two levels down and a second private pipe must be creatable
  // from inside an already-confined descendant.
  const relay = require('path').join(dir, 'shim-relay.cjs');
  fs.writeFileSync(
    relay,
    "const cp = require('child_process');\n" +
      'const g = cp.fork(' +
      JSON.stringify(forkee) +
      ", [], { stdio: 'inherit' });\n" +
      "g.on('message', (m) => { process.send({ relayed: m.pong }); g.kill(); });\n" +
      "process.on('message', (m) => g.send({ ping: m.ping }));\n",
  );

  const converse = (name, entry, payload) =>
    new Promise((resolve) => {
      let child;
      const finish = (line) => {
        one('op:' + name + '=' + line);
        try {
          child && child.kill();
        } catch (e) {}
        resolve();
      };
      try {
        child = cp.fork(entry, [], { stdio: 'inherit' });
      } catch (e) {
        finish('ERR ' + ((e && e.code) || 'throw') + ' ' + String((e && e.message) || e).slice(0, 160));
        return;
      }
      const guard = setTimeout(() => finish('ERR HUNG no reply within 12s'), 12000);
      child.once('error', (e) => {
        clearTimeout(guard);
        finish('ERR ' + ((e && e.code) || 'throw') + ' ' + String((e && e.message) || e).slice(0, 160));
      });
      child.once('message', (m) => {
        clearTimeout(guard);
        finish('OK reply=' + JSON.stringify(m));
      });
      child.send(payload);
    });

  one('child:shimfork-preswap arm=' + ARM);
  await converse('shim-fork-roundtrip', forkee, { ping: 'p1' });
  await converse('shim-fork-nested', relay, { ping: 'p2' });
  one('child:shimfork-done arm=' + ARM);
}

async function main() {
  if (process.env.BT_OBJ_MODE === 'fork') return forkMode();
  if (process.env.BT_OBJ_MODE === 'localpipe') return localPipeMode();
  if (process.env.BT_OBJ_MODE === 'shimfork') return shimForkMode();

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
