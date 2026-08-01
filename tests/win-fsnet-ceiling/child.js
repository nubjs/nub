// Runs INSIDE the arm's child process. Reads its op list from the file named by `FX_OPS` (written
// by the unconfined parent) and prints one `op:<id>=<result>` line per op.
//
// WHY THE OPS COME FROM A FILE: every arm runs the byte-identical program over the byte-identical
// op list, so a difference between arms is a difference of TOKEN, never of program.
//
// NO `child_process` ANYWHERE. A piped `spawnSync` never returns under an AppContainer
// (MECHANISM-FACTS §5h, reproduced across three runs) and swallows every op after it, which
// presents as a timed-out arm rather than as a failed op.
//
// NO `fs.realpath*`, and no bare-specifier `require`. Every realpath surface Node exposes is
// refused under an AppContainer (§5h's API table), and `Module._findPath` realpaths every
// non-main resolution — so this file uses only `node:`-prefixed builtins, which resolve without
// touching the filesystem, and absolute paths.

'use strict';
const fs = require('node:fs');
const net = require('node:net');
const dns = require('node:dns');
const https = require('node:https');

// Written IMMEDIATELY, never buffered: a child that dies mid-matrix must still leave every op it
// completed. Buffering turns a partial run into an empty log, which reads as "the mechanism is
// unavailable" rather than "op 7 killed it". fd 1 is the log handle the parent opened UNCONFINED
// and passed in already-open, so the child logs even when it can open nothing itself.
function say(k, v) {
  fs.writeSync(1, `${k}=${v}\n`);
}

// The CODE is the finding. `EPERM`/`EACCES` is the jail, `ENOENT` is a broken fixture, `EEXIST` is
// a name collision with an earlier arm — reporting only "failed" makes those indistinguishable.
// `errno` is carried too because libuv's numeric code is what maps back to a Win32 status.
function err(e) {
  if (!e) return 'ERR:unknown';
  return `ERR:${e.code || e.message}${typeof e.errno === 'number' ? `(${e.errno})` : ''}`;
}

const ops = JSON.parse(fs.readFileSync(process.env.FX_OPS, 'utf8'));

say('child:start', 'ok');
say('child:pid', process.pid);

for (const op of ops) {
  let res;
  try {
    switch (op.kind) {
      // CREATE a new file in an existing directory — the prompt's "write" cell. Deliberately
      // distinct from `mkdir`: the default DACL on `C:\` and `C:\ProgramData` grants
      // BUILTIN\Users "create folders / append data" but NOT "create files", so conflating the
      // two produces exactly the equivocation this probe exists to settle.
      case 'create':
        fs.writeFileSync(op.path, 'fx\n', { flag: 'wx' });
        res = 'OK';
        break;
      case 'mkdir':
        fs.mkdirSync(op.path, { recursive: false });
        res = 'OK';
        break;
      case 'read':
        res = `OK:${fs.readFileSync(op.path).length}`;
        break;
      case 'list':
        res = `OK:${fs.readdirSync(op.path).length}`;
        break;
      default:
        res = 'ERR:unknown-kind';
    }
  } catch (e) {
    res = err(e);
  }
  say(`op:${op.id}`, res);
}

// The NET axis, last, and in the SAME child as the filesystem matrix — that adjacency IS the
// separability question. A broad filesystem grant is a DACL fact; egress denial is a capability
// fact; if they are independent then both hold at once, in one token.
function once(fn) {
  let done = false;
  return (...a) => {
    if (done) return;
    done = true;
    fn(...a);
  };
}

function tcp() {
  return new Promise((resolve) => {
    // A literal IP, so a refusal is the capability and not name resolution.
    const fin = once((v) => resolve(['net:tcp-1.1.1.1:443', v]));
    const s = net.connect({ host: '1.1.1.1', port: 443 });
    const t = setTimeout(() => {
      s.destroy();
      fin('ERR:TIMEOUT');
    }, 10000);
    s.on('connect', () => {
      clearTimeout(t);
      s.destroy();
      fin('OK');
    });
    s.on('error', (e) => {
      clearTimeout(t);
      fin(err(e));
    });
  });
}

function lookup() {
  return new Promise((resolve) => {
    const fin = once((v) => resolve(['net:dns', v]));
    const t = setTimeout(() => fin('ERR:TIMEOUT'), 10000);
    dns.lookup('registry.npmjs.org', (e, addr) => {
      clearTimeout(t);
      fin(e ? err(e) : `OK:${addr}`);
    });
  });
}

// A real HTTP GET, not just a socket: TLS and the resolver are separate surfaces from `connect`,
// and a tier that leaks only one of them is still a leaking tier.
function httpGet() {
  return new Promise((resolve) => {
    const fin = once((v) => resolve(['net:https-get', v]));
    const t = setTimeout(() => fin('ERR:TIMEOUT'), 15000);
    try {
      const req = https.get('https://registry.npmjs.org/-/ping', (res) => {
        let n = 0;
        res.on('data', (c) => {
          n += c.length;
        });
        res.on('end', () => {
          clearTimeout(t);
          fin(`OK:${res.statusCode}:${n}`);
        });
      });
      req.on('error', (e) => {
        clearTimeout(t);
        fin(err(e));
      });
    } catch (e) {
      clearTimeout(t);
      fin(err(e));
    }
  });
}

Promise.all([tcp(), lookup(), httpGet()]).then((rows) => {
  for (const [k, v] of rows) say(k, v);
  // The completion marker. An arm whose log lacks it was truncated or hung, and its ops must NOT
  // be read as denials — the failure mode §5h lost a whole run to.
  say('child:done', 'ok');
});
