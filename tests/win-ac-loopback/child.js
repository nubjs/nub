// Node fallback child for the same-package loopback probe (W1).
//
// SELECTED ONLY IF `child.exe` CANNOT RUN CONFINED. It emits the same line keys as `child.cs`
// so `probe.ps1` parses one format, and it is the PROVEN runtime -- `tests/win-fsnet-ceiling`
// ran node children inside AppContainers on both images and is where the prior
// `connect 127.0.0.1:135 -> ETIMEDOUT` measurement came from.
//
// TWO THINGS IT CANNOT DO, which is why it is the fallback and not the default:
//   - It cannot read its OWN token, so `self:packageSid` / `self:capabilities` come only from
//     the launcher's process-handle read-back. MECHANICS 5i is explicit that a probe which does
//     not read the child's token proved nothing; one reading instead of two is weaker evidence.
//   - libuv collapses distinct Winsock statuses onto one errno, so there is no raw WSA number.
//     `errno` below is libuv's NEGATIVE code (ETIMEDOUT is -4039 on Windows), not a WSA code.
//
// `--preserve-symlinks-main` is passed by the launcher, not chosen here: an unflagged confined
// `node` dies in `resolveMainPath`'s realpath with `EPERM lstat 'C:\'` before running a line
// (MECHANISM-FACTS 5h). Only `node:`-prefixed builtins are imported, so `_findPath` is never
// entered and the tree-wide `--preserve-symlinks` is not needed.

'use strict';
const net = require('node:net');
const process = require('node:process');

function P(s) {
  // Synchronous write to fd 1 -- the parent polls this file for `listening=1` to sequence the
  // connector behind the listener, so a buffered ready line is a deadlock.
  require('node:fs').writeSync(1, s + '\n');
}

function reportToken(role) {
  P('self:role=' + role);
  P('self:pid=' + process.pid);
  P('self:isAppContainer=UNAVAILABLE-node-child');
  P('self:packageSid=UNAVAILABLE-node-child');
  P('self:capabilities=UNAVAILABLE-node-child');
  P('self:integrity=UNAVAILABLE-node-child');
}

function errLine(prefix, err) {
  return prefix + ' code=' + err.code + ' errno=' + err.errno + ' syscall=' + err.syscall;
}

function doListen(port) {
  const srv = net.createServer((sock) => {
    sock.setTimeout(5000);
    sock.once('data', (buf) => {
      P('listen:recv=' + buf.toString('ascii') + ' bytes=' + buf.length);
      sock.write('PONG');
      P('listen:sent=PONG');
      sock.end();
      srv.close();
      setTimeout(() => process.exit(0), 50);
    });
    P('listen:accept=OK peer=' + sock.remoteAddress + ':' + sock.remotePort);
  });
  srv.on('error', (err) => {
    P(errLine('listen:bind=FAILED', err));
    process.exit(21);
  });
  srv.listen(port, '127.0.0.1', () => {
    P('listen:bind=OK');
    P('listen:listen=OK');
    P('listen:listening=1 port=' + port);
  });
  setTimeout(() => {
    P('listen:accept=TIMEOUT-25s');
    process.exit(23);
  }, 25000).unref?.();
  // The unref above would let the process exit early if nothing else is pending; the server
  // handle keeps the loop alive, so the timer still fires. Kept explicit rather than relying on it.
  setTimeout(() => {}, 26000);
}

// roundTrip false is the EGRESS GATE control: connect to a public address and report, without a
// peer to talk to. It proves, inside this very run, that withholding internetClient is actually
// confining the child at the network layer -- so a CONNECTED loopback arm cannot be explained by
// "the AppContainer attribute was never applied". MECHANISM-FACTS 5l 4 measured it as EACCES.
function doConnect(host, port, roundTrip) {
  const t0 = Date.now();
  let settled = false;
  // TWO BOUNDS, matching child.cs: an outbound capability denial returns in single-digit ms,
  // while a receive-side DROP leaves the SYN unanswered until Windows' TCP retry budget runs
  // out (~21 s). A single 5 s ceiling reports both as "no completion" and erases the very
  // distinction MECHANISM-FACTS 5l 4 rests on.
  const at5s = setTimeout(() => {
    if (!settled) P('connect:at5s=PENDING');
  }, 5000);
  const hard = setTimeout(() => {
    if (settled) return;
    settled = true;
    P('connect:result=NO-COMPLETION-30s elapsedMs=' + (Date.now() - t0));
    process.exit(31);
  }, 30000);

  const sock = net.connect({ host, port }, () => {
    if (settled) return;
    settled = true;
    clearTimeout(at5s);
    clearTimeout(hard);
    P('connect:result=CONNECTED elapsedMs=' + (Date.now() - t0));
    if (!roundTrip) { sock.destroy(); process.exit(0); }
    sock.write('PING');
    P('connect:sent=PING');
    sock.setTimeout(5000);
    sock.once('data', (buf) => {
      const got = buf.toString('ascii');
      P('connect:recv=' + got + ' bytes=' + buf.length);
      P('connect:roundtrip=' + (got === 'PONG' ? 'OK' : 'MISMATCH'));
      sock.destroy();
      process.exit(got === 'PONG' ? 0 : 33);
    });
    sock.once('timeout', () => {
      P('connect:recv=TIMEOUT');
      process.exit(33);
    });
  });
  sock.on('error', (err) => {
    if (settled) return;
    settled = true;
    clearTimeout(at5s);
    clearTimeout(hard);
    P(errLine('connect:result=FAILED', err) + ' elapsedMs=' + (Date.now() - t0));
    process.exit(32);
  });
}

const mode = process.argv[2] || 'selftest';
const port = Number(process.argv[3] || 0);
reportToken(mode);
if (mode === 'selftest') {
  P('selftest:ok=1');
  process.exit(0);
} else if (mode === 'listen') {
  doListen(port);
} else if (mode === 'connect') {
  doConnect('127.0.0.1', port, true);
} else if (mode === 'egress') {
  doConnect('1.1.1.1', 443, false);
} else {
  P('mode:unknown=' + mode);
  process.exit(90);
}
